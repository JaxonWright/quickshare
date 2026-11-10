use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use serde::Serialize;
use tokio::process::Command;
use tokio::time::timeout;

use crate::{Event, Out};

const MOSEY: &str = "com.google.android.mosey";

#[derive(Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub fix: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Check { name, status, detail: detail.into(), fix: fix.into() }
    }
}

pub async fn run(out: &Out, port: u16, dir: &Path) -> ExitCode {
    let mut checks = vec![bluetooth().await, firewall(port), download_dir(dir)];
    checks.extend(phones().await);

    for c in &checks {
        out.emit(&Event::Check { name: c.name, status: c.status, detail: &c.detail, fix: &c.fix }, || {
            let mark = match c.status {
                Status::Ok => "ok  ",
                Status::Warn => "warn",
                Status::Fail => "FAIL",
            };
            let fix = if c.fix.is_empty() { String::new() } else { format!("\n      fix: {}", c.fix) };
            format!("{mark}  {}: {}{fix}", c.name, c.detail)
        });
    }
    if checks.iter().any(|c| c.status == Status::Fail) { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

async fn bluetooth() -> Check {
    const NAME: &str = "bluetooth";
    let adapter = async {
        let session = bluer::Session::new().await?;
        let adapter = session.default_adapter().await?;
        let powered = adapter.is_powered().await?;
        Ok::<_, bluer::Error>((adapter.name().to_string(), powered))
    };
    match timeout(Duration::from_secs(3), adapter).await {
        Ok(Ok((name, true))) => Check::new(NAME, Status::Ok, format!("{name} is powered"), ""),
        Ok(Ok((name, false))) => Check::new(NAME, Status::Warn,
            format!("{name} is off, so sending cannot wake a phone whose Quick Share screen is closed"),
            "rfkill unblock bluetooth && bluetoothctl power on"),
        _ => Check::new(NAME, Status::Warn,
            "no Bluetooth adapter answered over BlueZ; sending works only to phones already visible",
            "start bluetooth.service, or ignore this on a computer without Bluetooth"),
    }
}

// ufw stores each rule as a comment line: "### tuple ### allow tcp 53318 0.0.0.0/0 any 0.0.0.0/0 in".
fn ufw_allows(rules: &str, port: u16) -> bool {
    rules.lines().filter_map(|l| l.strip_prefix("### tuple ### ")).any(|tuple| {
        let f: Vec<&str> = tuple.split_whitespace().collect();
        f.len() >= 7
            && f[0] == "allow"
            && matches!(f[1], "tcp" | "any")
            && f[6].starts_with("in")
            && f[2].split(',').any(|spec| match spec.split_once(':') {
                Some((lo, hi)) => matches!((lo.parse::<u16>(), hi.parse::<u16>()), (Ok(lo), Ok(hi)) if lo <= port && port <= hi),
                None => spec == "any" || spec.parse() == Ok(port),
            })
    })
}

fn firewall(port: u16) -> Check {
    const NAME: &str = "firewall";
    let fix = format!("sudo ufw allow {port}/tcp comment quickshare");
    let active = std::fs::read_to_string("/etc/ufw/ufw.conf")
        .map(|c| c.lines().any(|l| l.trim() == "ENABLED=yes"))
        .unwrap_or(false);
    if !active {
        return Check::new(NAME, Status::Ok, "ufw is not enabled; other firewalls are not checked", "");
    }
    let accepts = std::fs::read_to_string("/etc/default/ufw")
        .map(|c| c.lines().any(|l| l.trim() == "DEFAULT_INPUT_POLICY=\"ACCEPT\""))
        .unwrap_or(false);
    let rules = std::fs::read_to_string("/etc/ufw/user.rules").unwrap_or_default();
    if accepts || ufw_allows(&rules, port) {
        Check::new(NAME, Status::Ok, format!("ufw allows incoming TCP {port}"), "")
    } else {
        Check::new(NAME, Status::Fail,
            format!("ufw drops incoming TCP {port}, so phones cannot send to this computer"), fix)
    }
}

fn download_dir(dir: &Path) -> Check {
    const NAME: &str = "downloads";
    let probe = dir.join(".quickshare-doctor");
    match std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&probe, b"")) {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Check::new(NAME, Status::Ok, format!("received files go to {}", dir.display()), "")
        }
        Err(e) => Check::new(NAME, Status::Fail, format!("cannot write to {}: {e}", dir.display()), "pass --dir to receive"),
    }
}

async fn adb(args: &[&str]) -> Option<String> {
    let run = Command::new("adb").args(args).kill_on_drop(true).output();
    let output = timeout(Duration::from_secs(5), run).await.ok()?.ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

// Pixel 10 and newer run Mosey, their AirDrop provider, when the Quick Share sheet opens, and it
// disconnects 5 GHz Wi-Fi. Only a phone connected over adb can be checked.
async fn phones() -> Vec<Check> {
    const NAME: &str = "phone";
    let Some(listing) = adb(&["devices"]).await else { return Vec::new() };
    let serials: Vec<&str> = listing
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once('\t'))
        .filter(|(_, state)| *state == "device")
        .map(|(serial, _)| serial)
        .collect();
    let mut checks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for serial in serials {
        // One phone can be listed twice, over its IP and its mDNS name.
        let hardware = adb(&["-s", serial, "shell", "getprop", "ro.serialno"]).await.unwrap_or_default();
        if !seen.insert(hardware.trim().to_string()) {
            continue;
        }
        let model = adb(&["-s", serial, "shell", "getprop", "ro.product.model"]).await.unwrap_or_default();
        let model = model.trim();
        let enabled = adb(&["-s", serial, "shell", "pm", "list", "packages", "-e", MOSEY]).await.unwrap_or_default();
        checks.push(if enabled.lines().any(|l| l.trim() == format!("package:{MOSEY}")) {
            Check::new(NAME, Status::Warn,
                format!("{model}: Mosey (AirDrop sharing) is on and drops 5 GHz Wi-Fi when Quick Share opens"),
                format!("adb -s {serial} shell pm disable-user --user 0 {MOSEY}"))
        } else {
            Check::new(NAME, Status::Ok, format!("{model}: no Mosey Wi-Fi conflict"), "")
        });
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::ufw_allows;

    #[test]
    fn reads_ufw_tuples() {
        let rules = "### tuple ### allow udp 53317 0.0.0.0/0 any 0.0.0.0/0 in\n\
                     ### tuple ### allow tcp 53318 0.0.0.0/0 any 0.0.0.0/0 in\n";
        assert!(ufw_allows(rules, 53318));
        assert!(!ufw_allows(rules, 53317), "a udp rule does not open tcp");
        assert!(ufw_allows("### tuple ### allow any 53000:54000 0.0.0.0/0 any 0.0.0.0/0 in\n", 53318));
        assert!(ufw_allows("### tuple ### allow tcp 22,53318 0.0.0.0/0 any 0.0.0.0/0 in\n", 53318));
        assert!(!ufw_allows("### tuple ### deny tcp 53318 0.0.0.0/0 any 0.0.0.0/0 in\n", 53318));
        assert!(!ufw_allows("### tuple ### allow tcp 53318 0.0.0.0/0 any 0.0.0.0/0 out\n", 53318));
        assert!(ufw_allows("### tuple ### allow tcp 53318 0.0.0.0/0 any 0.0.0.0/0 in comment=7175\n", 53318));
    }
}
