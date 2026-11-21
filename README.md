<p align="center">
  <img src="assets/logo.svg" alt="A goofy cartoon phone spilling photos into folders" width="200">
</p>

<h1 align="center">iphone-whatsapp-media-extractor</h1>

Pull the photos, videos, voice notes and documents out of WhatsApp on an iPhone, one folder per chat, with file dates set to when each message was sent and filenames that sort chronologically.

That is all it does. No chat export, no HTML viewer: other tools do that well.

## Install

### Download (macOS)

Grab the latest `iphone-whatsapp-media-extractor-<version>-macos-universal.tar.gz` (works on Apple Silicon and Intel) from the [Releases page](https://github.com/phanshiyu/iphone-whatsapp-media-extractor/releases/latest), then:

```sh
tar xzf iphone-whatsapp-media-extractor-*-macos-universal.tar.gz
cd iphone-whatsapp-media-extractor-*-macos-universal
xattr -c iphone-whatsapp-media-extractor  # clear quarantine: the binary is unsigned
sudo mv iphone-whatsapp-media-extractor /usr/local/bin/
```

### From source

```sh
cargo install --git https://github.com/phanshiyu/iphone-whatsapp-media-extractor
```

For the plug-in-the-phone mode you also need libimobiledevice (`idevicebackup2` and `idevice_id`):

```sh
brew install libimobiledevice          # macOS
sudo apt install libimobiledevice-utils # Debian / Ubuntu
```

## Usage

### Plug in the iPhone and run

```sh
iphone-whatsapp-media-extractor ~/WhatsAppMedia
```

The tool backs up the connected iPhone to a temporary folder, extracts the WhatsApp media, then deletes the backup (also on error and Ctrl-C). Unlock the phone and tap "Trust" or enter the passcode if asked. The **whole device** is backed up, so you need free disk space roughly equal to the used storage on the phone. Use `--work-dir <DIR>` to put the temporary backup on a bigger disk.

To keep the backup so later runs are incremental (much faster), use `--keep-backup <DIR>`. If several phones are connected, pick one with `--udid`.

### From an existing backup

```sh
iphone-whatsapp-media-extractor ~/WhatsAppMedia --backup "$HOME/Library/Application Support/MobileSync/Backup/<UDID>"
```

`--backup` accepts the `<UDID>` folder itself, or the parent `Backup` folder if it holds exactly one backup (otherwise the backups are listed so you can choose).

On macOS, Finder/iTunes backups live in `~/Library/Application Support/MobileSync/Backup/<UDID>`. Reading that folder needs Full Disk Access for your terminal (System Settings > Privacy & Security > Full Disk Access).

Encrypted backups work: you are prompted for the password, or pass `--password` / set `IWME_PASSWORD`. The backup is only ever read.

WhatsApp's own end-to-end encrypted chat backup is irrelevant here; this reads the iPhone's device backup, not WhatsApp's backup.

### Options

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

## Output

```
<OUTPUT_DIR>/<Chat Name>/YYYY-MM-DD_HH-MM-SS_NNN[_<sender>].<ext>
```

- Chat name is the contact or group name (falling back to the phone number), cleaned for all filesystems. Two chats that end up with the same name get ` (<number>)` appended.
- `NNN` numbers messages sent in the same second so sorting by name equals chronological order.
- In groups the sender is appended (`Me` for your own messages).
- The file's modified time (and creation time on macOS) is set to the message time.
- Re-running is safe: files that already exist with the same size are skipped, and their timestamps are repaired. (With `--exif`, exiftool changes file sizes, so those files are copied again on re-runs.)

A summary of chats, files extracted, files missing from the backup (referenced by WhatsApp but not present) and skipped files is printed at the end.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test
```

The tests build synthetic encrypted and unencrypted backups; no real phone data is needed.

## License

MIT
