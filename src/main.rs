use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use rqs_lib::channel::{ChannelAction, ChannelDirection, ChannelMessage, TransferType};
use rqs_lib::{DeviceType, EndpointInfo, OutboundPayload, SendInfo, State, Visibility, RQS};
use serde::Serialize;
use tokio::io::AsyncBufReadExt;
use tokio::sync::broadcast;
use tokio::time::Instant;

#[derive(Parser)]
#[command(name = "quickshare", version, about = "Send and receive files with Android Quick Share")]
struct Cli {
    /// Print one JSON object per line instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List nearby devices that are visible to Quick Share.
    Browse {
        #[arg(long, default_value_t = 10)]
        timeout: u64,
    },
    /// Send files to a device found by `browse`, by its id or name.
    Send {
        #[arg(long)]
        to: String,
        /// Seconds to find the device and wait for its owner to answer.
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Stay visible and receive files until interrupted.
    Receive {
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Accept every incoming transfer without asking.
        #[arg(long)]
        yes: bool,
        /// Exit after the first finished transfer.
        #[arg(long)]
        once: bool,
        /// TCP port phones connect to; allow it through the firewall.
        #[arg(long, default_value_t = 53318)]
        port: u16,
    },
}

#[derive(Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Event<'a> {
    Device { id: &'a str, name: &'a str, kind: &'static str, addr: &'a str },
    Gone { id: &'a str },
    Request { id: &'a str, from: &'a str, pin: &'a str, files: &'a [String], bytes: u64 },
    Progress { id: &'a str, done: u64, total: u64 },
    Received { id: &'a str, files: &'a [String], dir: &'a str },
    Result { id: &'a str, code: Outcome },
}

#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Outcome {
    Delivered,
    Declined,
    Cancelled,
    Failed,
}

impl Outcome {
    fn from_state(state: &State) -> Option<Self> {
        match state {
            State::Finished => Some(Outcome::Delivered),
            State::Rejected => Some(Outcome::Declined),
            State::Cancelled => Some(Outcome::Cancelled),
            State::Disconnected => Some(Outcome::Failed),
            _ => None,
        }
    }

    fn exit(self) -> ExitCode {
        match self {
            Outcome::Delivered => ExitCode::SUCCESS,
            Outcome::Declined | Outcome::Cancelled => ExitCode::from(1),
            Outcome::Failed => ExitCode::from(2),
        }
    }
}

struct Out {
    json: bool,
}

impl Out {
    fn emit(&self, event: &Event, text: impl FnOnce() -> String) {
        let line = if self.json { serde_json::to_string(event).expect("events serialize") } else { text() };
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

#[derive(Clone)]
struct Device {
    id: String,
    name: String,
    addr: String,
    kind: &'static str,
}

impl Device {
    fn parse(info: &EndpointInfo) -> Option<Self> {
        let addr = format!("{}:{}", info.ip.as_ref()?, info.port.as_ref()?);
        let kind = match info.rtype {
            Some(DeviceType::Phone) => "phone",
            Some(DeviceType::Tablet) => "tablet",
            Some(DeviceType::Laptop) => "laptop",
            _ => "unknown",
        };
        Some(Device { id: info.id.clone(), name: info.name.clone().unwrap_or_else(|| info.id.clone()), addr, kind })
    }

    fn matches(&self, wanted: &str) -> bool {
        self.id == wanted || self.name.eq_ignore_ascii_case(wanted)
    }
}

fn report_device(out: &Out, d: &Device) {
    out.emit(&Event::Device { id: &d.id, name: &d.name, kind: d.kind, addr: &d.addr }, || {
        format!("{}\t{}\t{}", d.id, d.name, d.kind)
    });
}

fn is_lib_message(m: &ChannelMessage, rtype: TransferType) -> bool {
    m.direction == ChannelDirection::LibToFront && m.rtype.as_ref() == Some(&rtype)
}

async fn browse(out: &Out, timeout: u64) -> anyhow::Result<ExitCode> {
    let mut rqs = RQS::new(Visibility::Invisible, None, None);
    rqs.run().await?;
    let (tx, mut rx) = broadcast::channel(64);
    rqs.discovery(tx)?;

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut seen: HashMap<String, Device> = HashMap::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            _ = tokio::signal::ctrl_c() => break,
            info = rx.recv() => {
                let Ok(info) = info else { continue };
                if info.present == Some(false) {
                    if seen.remove(&info.id).is_some() {
                        out.emit(&Event::Gone { id: &info.id }, || format!("gone\t{}", info.id));
                    }
                } else if let Some(d) = Device::parse(&info) {
                    if !seen.contains_key(&d.id) {
                        report_device(out, &d);
                        seen.insert(d.id.clone(), d);
                    }
                }
            }
        }
    }
    rqs.stop().await;
    Ok(ExitCode::SUCCESS)
}

async fn send(out: &Out, to: &str, timeout: u64, files: Vec<PathBuf>) -> anyhow::Result<ExitCode> {
    let mut paths = Vec::with_capacity(files.len());
    for f in &files {
        let abs = std::fs::canonicalize(f).with_context(|| format!("{} cannot be read", f.display()))?;
        if !abs.is_file() {
            bail!("{} is not a file", f.display());
        }
        paths.push(abs.to_string_lossy().into_owned());
    }

    let mut rqs = RQS::new(Visibility::Invisible, None, None);
    let (sender, _) = rqs.run().await?;
    let mut messages = rqs.message_sender.subscribe();
    let (tx, mut found) = broadcast::channel(64);
    rqs.discovery(tx)?;

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let outcome = 'run: {
        let device = loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    eprintln!("quickshare: no device named {to} was found");
                    break 'run Outcome::Failed;
                }
                info = found.recv() => {
                    let Ok(info) = info else { continue };
                    if let Some(d) = Device::parse(&info).filter(|d| d.matches(to)) { break d; }
                }
            }
        };
        rqs.stop_discovery();
        report_device(out, &device);
        sender
            .send(SendInfo { id: device.id.clone(), name: device.name.clone(), addr: device.addr.clone(), ob: OutboundPayload::Files(paths) })
            .await?;

        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    eprintln!("quickshare: {} did not answer in time", device.name);
                    break 'run Outcome::Failed;
                }
                _ = tokio::signal::ctrl_c() => break 'run Outcome::Cancelled,
                m = messages.recv() => {
                    let Ok(m) = m else { continue };
                    if !is_lib_message(&m, TransferType::Outbound) { continue; }
                    let Some(state) = &m.state else { continue };
                    if *state == State::SendingFiles {
                        if let Some(meta) = &m.meta {
                            out.emit(&Event::Progress { id: &m.id, done: meta.ack_bytes, total: meta.total_bytes },
                                || format!("{} / {} bytes", meta.ack_bytes, meta.total_bytes));
                        }
                    }
                    if let Some(code) = Outcome::from_state(state) {
                        out.emit(&Event::Result { id: &m.id, code }, || format!("{code:?}").to_lowercase());
                        break 'run code;
                    }
                }
            }
        }
    };
    rqs.stop().await;
    Ok(outcome.exit())
}

fn download_dir(dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let dir = dir
        .or_else(|| std::env::var_os("XDG_DOWNLOAD_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")))
        .context("no download folder: pass --dir")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir)
}

async fn ask(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let mut line = String::new();
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    stdin.read_line(&mut line).await.is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

async fn receive(out: &Out, dir: Option<PathBuf>, yes: bool, once: bool, port: u16) -> anyhow::Result<ExitCode> {
    let dir = download_dir(dir)?;
    let dir_text = dir.to_string_lossy().into_owned();
    let interactive = !yes && std::io::stdin().is_terminal();
    let mut rqs = RQS::new(Visibility::Visible, Some(port.into()), Some(dir));
    rqs.run().await?;
    let mut messages = rqs.message_sender.subscribe();
    let reply = rqs.message_sender.clone();
    if !out.json {
        eprintln!("quickshare: visible on TCP port {port}; receiving into {dir_text}");
    }

    let mut files: HashMap<String, Vec<String>> = HashMap::new();
    loop {
        let m = tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            m = messages.recv() => match m { Ok(m) => m, Err(_) => continue },
        };
        if !is_lib_message(&m, TransferType::Inbound) { continue; }
        let (Some(state), Some(meta)) = (&m.state, &m.meta) else { continue };
        match state {
            State::WaitingForUserConsent => {
                let from = meta.source.as_ref().map(|s| s.name.as_str()).unwrap_or("an unknown device");
                let names = meta.files.clone().unwrap_or_default();
                let pin = meta.pin_code.as_deref().unwrap_or("");
                out.emit(&Event::Request { id: &m.id, from, pin, files: &names, bytes: meta.total_bytes }, || {
                    format!("{from} wants to send {} ({} bytes), PIN {pin}", names.join(", "), meta.total_bytes)
                });
                let accept = yes || (interactive && ask("Accept?").await);
                files.insert(m.id.clone(), names);
                let action = if accept { ChannelAction::AcceptTransfer } else { ChannelAction::RejectTransfer };
                let _ = reply.send(ChannelMessage { id: m.id.clone(), direction: ChannelDirection::FrontToLib, action: Some(action), ..Default::default() });
            }
            State::ReceivingFiles => out.emit(&Event::Progress { id: &m.id, done: meta.ack_bytes, total: meta.total_bytes },
                || format!("{} / {} bytes", meta.ack_bytes, meta.total_bytes)),
            State::Finished => {
                let names = files.remove(&m.id).unwrap_or_default();
                out.emit(&Event::Received { id: &m.id, files: &names, dir: &dir_text }, || {
                    format!("received {} into {dir_text}", names.join(", "))
                });
                if once { break; }
            }
            other => if let Some(code) = Outcome::from_state(other) {
                files.remove(&m.id);
                out.emit(&Event::Result { id: &m.id, code }, || format!("{code:?}").to_lowercase());
            }
        }
    }
    rqs.stop().await;
    Ok(ExitCode::SUCCESS)
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("QUICKSHARE_LOG").unwrap_or_else(|_| "off".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let out = Out { json: cli.json };
    let result = match cli.command {
        Command::Browse { timeout } => browse(&out, timeout).await,
        Command::Send { to, timeout, files } => send(&out, &to, timeout, files).await,
        Command::Receive { dir, yes, once, port } => receive(&out, dir, yes, once, port).await,
    };
    result.unwrap_or_else(|e| {
        eprintln!("quickshare: {e:#}");
        ExitCode::from(2)
    })
}
