# quickshare

A command-line client for Android Quick Share on Linux. It lists nearby devices, sends files to an Android phone, and receives files from one. Every command can print one JSON object per line, so a file manager, a bar widget or a script can drive it without linking to it. [flea-quickshare](https://github.com/JaxonWright/flea-quickshare) uses it to add Quick Share to the [Flea](https://github.com/thisisgm/flea) file menu.

quickshare runs the Quick Share protocol through `rqs_lib`, the library inside [rquickshare](https://github.com/Martichou/rquickshare) by Martichou. A patched copy lives in `vendor/rqs_lib`. See [Changes to rqs_lib](#changes-to-rqs_lib).

## Requirements

- Linux with BlueZ and a Bluetooth adapter. Sending uses a Bluetooth advertisement to wake the phone's Quick Share. Receiving works without Bluetooth.
- The computer and the phone on the same network, with multicast DNS allowed. Public Wi-Fi often blocks it.
- To build, Rust, `protoc` and the D-Bus headers. It was built and tested with Rust 1.98.1.

## Build and install

```sh
cargo build --release
install -Dm755 target/release/quickshare ~/.local/bin/quickshare
```

## Allow the receive port through the firewall

A phone that sends to this computer opens a TCP connection to it. `quickshare receive` listens on port 53318 unless you pass `--port`. A firewall that denies incoming connections, such as Omarchy's ufw setup, drops the connection silently, and the phone reports that the transfer failed. Allow the port once:

```sh
sudo ufw allow 53318/tcp comment quickshare
```

`browse` and `send` only make outgoing connections and need no rule.

## Usage

```sh
quickshare browse
quickshare send --to "Jaxon's Pixel" photo.jpg notes.pdf
quickshare receive
```

`browse` listens for 10 seconds and prints each device it finds. Change that with `--timeout`. The phone has to be visible to the computer, which usually means its Quick Share screen is open or its visibility is set to Everyone. With the screen open, a Pixel 11 Pro Fold appeared within 1.5 seconds in testing.

`send --to` takes a device name, which it matches without regard to case, or the id that `browse` printed. It waits up to 120 seconds for the device to appear and for its owner to tap Accept. Change that with `--timeout`. It prints byte progress while it sends.

`receive` makes the computer visible and saves files to the Download folder named in `$XDG_DOWNLOAD_DIR` or `~/.config/user-dirs.dirs`, or `~/Downloads` when neither names one. Choose another folder with `--dir`. For each incoming transfer it prints the sender's name and a PIN, which matches the PIN on the phone. In a terminal it asks before accepting. Without a terminal it declines every transfer, unless you pass `--yes`, so a stranger nearby cannot write files to the computer. `--once` exits after the first finished transfer.

## Check the setup

```sh
quickshare doctor
```

`doctor` prints one line per check, with the command that fixes it. It exits 1 when a check fails.

- **bluetooth** warns when no adapter answers over BlueZ or the adapter is off. Sending still works to a phone whose Quick Share screen is open.
- **firewall** fails when ufw is enabled and drops incoming TCP on the receive port. It reads the ufw rule files and checks no other firewall.
- **downloads** fails when the folder `receive` would write to is not writable.
- **phone** runs only for phones connected over adb. It warns when the Mosey app described below is enabled.

Pass `--port` and `--dir` to check the values you give `receive`.

### Exit status of send

| Status | Meaning |
| --- | --- |
| 0 | The phone received every file. |
| 1 | The owner declined, let the prompt expire, or cancelled during the transfer. |
| 2 | The device was not found, the connection failed, or the arguments were wrong. |

Errors print one line on stderr that starts with `quickshare:`.

### JSON output

`--json` makes every command print one object per line on stdout. The `t` field names the event.

| `t` | Fields | Printed by |
| --- | --- | --- |
| `device` | `id`, `name`, `kind` (`phone`, `tablet`, `laptop` or `unknown`), `addr` | `browse`, and `send` once it finds the device |
| `gone` | `id` | `browse` |
| `request` | `id`, `from`, `pin`, `files`, `bytes` | `receive` |
| `progress` | `id`, `done`, `total` in bytes | `send`, `receive` |
| `received` | `id`, `files`, `dir` | `receive` |
| `result` | `id`, `code` (`delivered`, `declined`, `cancelled` or `failed`) | `send`, `receive` |

```json
{"t":"device","id":"10.0.1.164:53601","name":"Jaxon's Pixel 11 Pro Fold","kind":"phone","addr":"10.0.1.164:53601"}
{"t":"progress","id":"10.0.1.164:53601","done":524288,"total":1572309}
{"t":"result","id":"10.0.1.164:53601","code":"delivered"}
```

Set `QUICKSHARE_LOG=debug` to print rqs_lib's log on stderr.

## Pixel 10 and newer drop Wi-Fi when Quick Share opens

Pixel phones that can share with AirDrop run a system app, `com.google.android.mosey`, whenever the Quick Share sheet opens. When the phone's Wi-Fi uses the same band that Mosey needs, Mosey disconnects Wi-Fi. The phone's log says `Disconnect Wi-Fi because STA and mosey are running on the same band`. Without Wi-Fi the phone cannot reach the computer.

The phone's settings have no switch for this. There are two workarounds:

- Disable Mosey over adb. This removes AirDrop sharing from the phone and nothing else. Tested on a Pixel 11 Pro Fold.

  ```sh
  adb shell pm disable-user --user 0 com.google.android.mosey
  adb shell pm enable com.google.android.mosey   # to undo
  ```

- Connect the phone to a 2.4 GHz or 6 GHz network instead of 5 GHz. This follows from the log message and is untested.

## Changes to rqs_lib

`vendor/rqs_lib` is `core_lib` from rquickshare at the commit in `vendor/rqs_lib/UPSTREAM`, with these changes to sending in `src/hdl/outbound.rs` and `src/manager.rs`:

- The upstream sender writes every chunk, sends a disconnection frame and closes the socket at once. Android queues chunks on a reader thread and discards the queue when the sender closes first, so the phone kept only the first 512 KiB of each file and the sender still reported success. The patched sender waits for the phone to send its disconnection frame and close the connection, and reports success only then.
- The sender now reads the phone's frames between chunks. It answers keep-alive frames and stops when the phone sends a payload cancel or error control message. Upstream rejected those control messages as malformed.
- A declined or expired consent prompt now ends in `Rejected` instead of `Disconnected`.
- A send that fails after the connection opens now reports itself as an outbound transfer, so a client can tell which transfer failed.

## License

GPL-3.0-only, because rqs_lib is GPL-3.0. See `LICENSE`.
