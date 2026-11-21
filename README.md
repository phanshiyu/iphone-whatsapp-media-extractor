<p align="center">
  <img src="assets/logo.png" alt="A goofy cartoon phone spilling photos into folders" width="160">
</p>

<h1 align="center">iPhone WhatsApp Media Extractor</h1>

Pull the photos, videos, voice notes and documents out of WhatsApp on an iPhone, one folder per chat, with file dates set to when each message was sent. A small Mac app, plus a command-line tool.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/phanshiyu/iphone-whatsapp-media-extractor/main/install.sh | sh
```

## Update

Same command again. It replaces the existing install and prints `Updated to vX`.

```sh
curl -fsSL https://raw.githubusercontent.com/phanshiyu/iphone-whatsapp-media-extractor/main/install.sh | sh
```

## Uninstall

```sh
curl -fsSL https://raw.githubusercontent.com/phanshiyu/iphone-whatsapp-media-extractor/main/install.sh | sh -s -- --uninstall
```

The installer puts the app in `/Applications` (or `~/Applications`), the `iphone-whatsapp-media-extractor` command in `/usr/local/bin` (or `~/.local/bin`), and installs `libimobiledevice` with Homebrew if it is missing. Flags: `--no-deps`, `--cli-only`, `--uninstall`. No sudo needed.

## Using the app

1. Open **iPhone WhatsApp Media Extractor** from Applications or Launchpad.
2. Pick a source: **Back up my connected iPhone** (plug it in, unlock it, tap "Trust"), or **Use an existing backup** from Finder.
3. Choose where to **Save to** (default `~/Desktop/WhatsApp Media`) and enter the backup password if asked.
4. Click **Extract**. Progress is shown; you can cancel any time.
5. Click **Show in Finder** when it is done.

## Command line

```sh
iphone-whatsapp-media-extractor ~/WhatsAppMedia                  # back up the connected iPhone, extract, delete the backup
iphone-whatsapp-media-extractor ~/WhatsAppMedia --backup "$HOME/Library/Application Support/MobileSync/Backup/<UDID>"
```

Device mode backs up the **whole phone** to a temporary folder (free space roughly equal to the phone's used storage; `--work-dir` picks another disk) and deletes it afterwards, also on error and Ctrl-C. `--keep-backup <DIR>` keeps it so later runs are incremental. `--backup` accepts the `<UDID>` folder, or the parent folder if it holds exactly one backup. Encrypted backups: you are prompted, or use `--password` / `IWME_PASSWORD`.

| Flag | Meaning |
| --- | --- |
| `--backup <DIR>` | Use an existing backup instead of backing up the phone |
| `--keep-backup <DIR>` | Keep the device backup in `DIR` (incremental re-runs) |
| `--udid <UDID>` | Which device to back up if several are connected |
| `--work-dir <DIR>` | Where to put the temporary backup |
| `--password <PW>` | Backup password (also `IWME_PASSWORD`) |
| `--business` | WhatsApp Business instead of WhatsApp |
| `--include-status` | Also extract status updates (skipped by default) |
| `--utc` | Use UTC instead of local time in file names |
| `--dry-run` | Show what would be done without writing |
| `--exif` | If `exiftool` is installed, also write date tags (jpg, jpeg, heic, png, mp4, mov) |

From source: `cargo install --git https://github.com/phanshiyu/iphone-whatsapp-media-extractor iphone-whatsapp-media-extractor`

## Notes

- **Full Disk Access** is only needed to read existing Finder backups (`~/Library/Application Support/MobileSync/Backup`). The app shows a banner with an "Open Settings" button: enable it for the app, then reopen it. For the CLI, enable it for your terminal. Backing up a connected iPhone does not need it.
- **Device mode needs libimobiledevice** (`brew install libimobiledevice`); the installer sets it up. Existing-backup mode works without it.
- **The app is not notarized** (ad-hoc signed). Installed with the command above it simply opens. If you download the `.dmg` or `.zip` from the Releases page by hand, macOS may block it: open System Settings > Privacy & Security and click **Open Anyway**.
- WhatsApp's own end-to-end encrypted chat backup is irrelevant: this reads the iPhone's device backup.

## Output

```
<OUTPUT_DIR>/<Chat Name>/YYYY-MM-DD_HH-MM-SS_NNN[_<sender>].<ext>
```

- Chat name is the contact or group name (falling back to the phone number), cleaned for all filesystems. Chats with the same name get ` (<number>)` appended.
- `NNN` numbers messages sent in the same second so sorting by name is chronological. In groups the sender is appended (`Me` for your own messages).
- File modified (and, on macOS, creation) times are set to the message time.
- Re-running is safe: files already present with the same size are skipped and their timestamps repaired. (With EXIF writing, exiftool changes sizes, so those files are copied again.)

## Development

Cargo workspace: `crates/core` (`iwme-core`, library), `crates/cli` (command line), `crates/app` (`iwme-app`, egui GUI).

```sh
cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace
cargo run -p iwme-app                                    # the GUI
scripts/bundle-macos.sh <binary> <version> <out-dir>     # build the .app
```

Tests build synthetic backups; no real phone data is needed. Releases are built by tagging `v*`.

## License

MIT
