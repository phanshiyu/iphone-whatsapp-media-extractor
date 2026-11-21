//! Taking a backup from a connected iPhone via libimobiledevice, and locating
//! existing backups.

use crate::error::access_error;
use crate::tools::{find_tool, home_dir};
use crate::{CancelToken, Progress, ProgressFn};
use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

/// Program that lists connected devices.
pub const IDEVICE_ID: &str = "idevice_id";
/// Program that reads device properties.
pub const IDEVICEINFO: &str = "ideviceinfo";
/// Program that takes the backup.
pub const IDEVICEBACKUP2: &str = "idevicebackup2";

/// What to tell the user when libimobiledevice is missing.
pub const INSTALL_HINT: &str = "install libimobiledevice (macOS: `brew install libimobiledevice`; Debian/Ubuntu: `apt install libimobiledevice-utils`)";

/// Settings for device mode.
#[derive(Debug, Clone, Default)]
pub struct DeviceOptions {
    /// Device to back up when several are connected.
    pub udid: Option<String>,
    /// Persistent backup folder (re-runs are incremental; never deleted).
    pub keep_backup: Option<PathBuf>,
    /// Where to put the temporary backup (defaults to the system temp dir).
    pub work_dir: Option<PathBuf>,
}

/// A backup folder that is removed on drop if it is temporary.
#[derive(Debug)]
pub struct BackupSource {
    /// Folder containing `Manifest.plist`.
    pub path: PathBuf,
    _temp: Option<TempDir>,
}

impl BackupSource {
    /// Wrap an existing backup that must not be deleted.
    #[must_use]
    pub const fn existing(path: PathBuf) -> Self {
        Self { path, _temp: None }
    }
}

/// Parse the output of `idevice_id -l`.
#[must_use]
pub fn parse_device_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Choose the device to back up.
pub fn select_device(devices: &[String], wanted: Option<&str>) -> Result<String> {
    match (wanted, devices) {
        (Some(w), _) => {
            if devices.iter().any(|d| d == w) {
                Ok(w.to_owned())
            } else {
                bail!(
                    "device {w} is not connected (connected: {})",
                    list_or_none(devices)
                )
            }
        }
        (None, []) => bail!(
            "no iPhone found: connect it with a cable, unlock it and tap \"Trust This Computer\""
        ),
        (None, [one]) => Ok(one.clone()),
        (None, many) => bail!(
            "several devices connected ({}); choose one with --udid",
            many.join(", ")
        ),
    }
}

fn list_or_none(devices: &[String]) -> String {
    if devices.is_empty() {
        "none".to_owned()
    } else {
        devices.join(", ")
    }
}

/// Arguments for `idevicebackup2`.
#[must_use]
pub fn backup_args(udid: &str, dir: &Path, full: bool) -> Vec<String> {
    let mut args = vec!["-u".to_owned(), udid.to_owned(), "backup".to_owned()];
    if full {
        args.push("--full".to_owned());
    }
    args.push(dir.display().to_string());
    args
}

/// Whether `dir` already holds a backup of `udid`.
#[must_use]
pub fn has_previous_backup(dir: &Path, udid: &str) -> bool {
    dir.join(udid).join("Manifest.plist").is_file()
}

/// Resolve `--backup`: either the backup folder itself, or a parent holding
/// exactly one backup.
pub fn find_backup(path: &Path) -> Result<PathBuf> {
    if path.join("Manifest.plist").is_file() {
        return Ok(path.to_owned());
    }
    let mut found: Vec<PathBuf> = fs::read_dir(path)
        .map_err(|e| access_error(e, &format!("cannot read {}", path.display())))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("Manifest.plist").is_file())
        .collect();
    found.sort();
    match found.as_slice() {
        [] => bail!(
            "no iPhone backup found in {} (no Manifest.plist)",
            path.display()
        ),
        [one] => Ok(one.clone()),
        many => {
            let lines: Vec<String> = many.iter().map(|p| describe_backup(p)).collect();
            bail!(
                "several backups found; pass one of these with --backup:\n{}",
                lines.join("\n")
            )
        }
    }
}

fn describe_backup(path: &Path) -> String {
    let info = plist::Value::from_file(path.join("Info.plist")).ok();
    let field = |key: &str| {
        info.as_ref()
            .and_then(|v| v.as_dictionary())
            .and_then(|d| d.get(key))
            .and_then(|v| match v {
                plist::Value::String(s) => Some(s.clone()),
                plist::Value::Date(d) => Some(d.to_xml_format()),
                _ => None,
            })
            .unwrap_or_else(|| "?".to_owned())
    };
    format!(
        "  {}  (device: {}, last backup: {})",
        path.display(),
        field("Device Name"),
        field("Last Backup Date")
    )
}

/// Resolve a libimobiledevice tool to an absolute path.
fn tool(name: &str) -> Result<PathBuf> {
    find_tool(name).ok_or_else(|| anyhow!("`{name}` was not found; {INSTALL_HINT}"))
}

/// The libimobiledevice programs that are not installed.
#[must_use]
pub fn missing_tools() -> Vec<&'static str> {
    [IDEVICE_ID, IDEVICEINFO, IDEVICEBACKUP2]
        .into_iter()
        .filter(|t| find_tool(t).is_none())
        .collect()
}

fn require_tools() -> Result<()> {
    for prog in [IDEVICE_ID, IDEVICEINFO, IDEVICEBACKUP2] {
        tool(prog)?;
    }
    Ok(())
}

/// A connected device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Unique device id.
    pub udid: String,
    /// User-visible name, if it could be read.
    pub name: Option<String>,
}

impl Device {
    /// Name for display, falling back to the UDID.
    #[must_use]
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.udid)
    }
}

fn list_udids() -> Result<Vec<String>> {
    let listing = Command::new(tool(IDEVICE_ID)?)
        .arg("-l")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {IDEVICE_ID}"))?;
    Ok(parse_device_list(&String::from_utf8_lossy(&listing.stdout)))
}

/// The device's name (`ideviceinfo -k DeviceName`), if readable.
#[must_use]
pub fn device_name(udid: &str) -> Option<String> {
    let out = Command::new(find_tool(IDEVICEINFO)?)
        .args(["-u", udid, "-k", "DeviceName"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// List connected devices with their names.
pub fn list_devices() -> Result<Vec<Device>> {
    require_tools()?;
    Ok(list_udids()?
        .into_iter()
        .map(|udid| Device {
            name: device_name(&udid),
            udid,
        })
        .collect())
}

/// An existing backup in a `MobileSync` folder.
#[derive(Debug, Clone)]
pub struct BackupInfo {
    /// Backup folder (contains `Manifest.plist`).
    pub path: PathBuf,
    /// Name of the device that was backed up.
    pub device_name: Option<String>,
    /// When the backup last completed.
    pub last_backup: Option<jiff::Timestamp>,
    /// Whether the backup is password protected.
    pub encrypted: bool,
}

/// Finder/iTunes backup folder on macOS.
#[must_use]
pub fn default_backup_root() -> Option<PathBuf> {
    home_dir().map(|h| h.join("Library/Application Support/MobileSync/Backup"))
}

/// Describe the backup at `path` from its `Info.plist` and `Manifest.plist`.
#[must_use]
pub fn backup_info(path: &Path) -> BackupInfo {
    let info = plist::Value::from_file(path.join("Info.plist")).ok();
    let dict = info.as_ref().and_then(plist::Value::as_dictionary);
    let device_name = dict
        .and_then(|d| d.get("Device Name"))
        .and_then(plist::Value::as_string)
        .map(str::to_owned);
    let last_backup = dict
        .and_then(|d| d.get("Last Backup Date"))
        .and_then(plist::Value::as_date)
        .and_then(|d| jiff::Timestamp::try_from(std::time::SystemTime::from(d)).ok());
    BackupInfo {
        path: path.to_owned(),
        device_name,
        last_backup,
        encrypted: crate::backup::Backup::is_encrypted(path).unwrap_or(false),
    }
}

/// List the backups inside `root`, newest first. A missing folder yields an
/// empty list; a permission error is a [`crate::FullDiskAccessRequired`].
pub fn list_backups(root: &Path) -> Result<Vec<BackupInfo>> {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(access_error(e, &format!("cannot read {}", root.display()))),
    };
    let mut found: Vec<BackupInfo> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("Manifest.plist").is_file())
        .map(|p| backup_info(&p))
        .collect();
    found.sort_by(|a, b| {
        b.last_backup
            .cmp(&a.last_backup)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(found)
}

/// Splits raw tool output into lines on `\n` and `\r` (progress bars
/// redraw with `\r`).
#[derive(Debug, Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    /// Feed bytes; returns the completed, non-empty lines.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &b in bytes {
            if b == b'\n' || b == b'\r' {
                lines.extend(self.take());
            } else {
                self.buf.push(b);
            }
        }
        lines
    }

    /// Flush a trailing partial line.
    pub fn finish(&mut self) -> Option<String> {
        self.take()
    }

    fn take(&mut self) -> Option<String> {
        let mut text = String::new();
        let mut in_escape = false;
        for c in String::from_utf8_lossy(&self.buf).chars() {
            if in_escape {
                in_escape = !c.is_ascii_alphabetic();
            } else if c == '\u{1b}' {
                in_escape = true;
            } else if !c.is_control() {
                text.push(c);
            }
        }
        self.buf.clear();
        let text = text.trim().to_owned();
        (!text.is_empty()).then_some(text)
    }
}

/// Best-effort: the last `NN%` / `NN.N%` in a line of `idevicebackup2` output.
#[must_use]
pub fn parse_percent(line: &str) -> Option<f32> {
    let chars: Vec<char> = line.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == '%')
        .rev()
        .find_map(|(i, _)| {
            let head = chars.get(..i)?;
            let start = head
                .iter()
                .rposition(|c| !(c.is_ascii_digit() || *c == '.'))
                .map_or(0, |p| p + 1);
            let num: String = head.get(start..)?.iter().collect();
            num.parse::<f32>()
                .ok()
                .filter(|p| (0.0..=100.0).contains(p))
        })
}

fn pump<R: Read + Send + 'static>(
    mut reader: R,
    id: usize,
    tx: mpsc::Sender<(usize, Vec<u8>)>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let Some(chunk) = buf.get(..n) else { break };
                    if tx.send((id, chunk.to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

fn emit_line(line: String, progress: ProgressFn<'_>) {
    progress(Progress::BackingUp {
        percent: parse_percent(&line),
        line,
    });
}

/// Run `program`, forwarding its output lines as progress and killing it when
/// `cancel` is set.
fn run_with_progress(
    program: &Path,
    args: &[String],
    cancel: &CancelToken,
    progress: ProgressFn<'_>,
) -> Result<ExitStatus> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run {}", program.display()))?;
    let (tx, rx) = mpsc::channel();
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(pump(out, 0, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(pump(err, 1, tx.clone()));
    }
    drop(tx);
    let mut splitters = [LineSplitter::default(), LineSplitter::default()];
    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(crate::Cancelled.into());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok((id, chunk)) => {
                if let Some(sp) = splitters.get_mut(id) {
                    for line in sp.push(&chunk) {
                        emit_line(line, progress);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    for sp in &mut splitters {
        if let Some(line) = sp.finish() {
            emit_line(line, progress);
        }
    }
    for r in readers {
        let _ = r.join();
    }
    Ok(child.wait()?)
}

/// Parse the output of `ideviceinfo -k WillEncrypt`.
#[must_use]
pub fn parse_will_encrypt(stdout: &str) -> Option<bool> {
    match stdout.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Whether the device will produce an encrypted backup (`None` if unknown).
#[must_use]
pub fn will_encrypt(udid: &str) -> Option<bool> {
    let out = Command::new(find_tool(IDEVICEINFO)?)
        .args([
            "-u",
            udid,
            "-q",
            "com.apple.mobile.backup",
            "-k",
            "WillEncrypt",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_will_encrypt(&String::from_utf8_lossy(&out.stdout)))
        .flatten()
}

/// Check the tools are installed and pick the device to back up.
pub fn pick_device(opts: &DeviceOptions) -> Result<String> {
    require_tools()?;
    select_device(&list_udids()?, opts.udid.as_deref())
}

/// The persistent backup folder for `udid`, if `--keep-backup` points at an existing one.
#[must_use]
pub fn existing_kept_backup(opts: &DeviceOptions, udid: &str) -> Option<PathBuf> {
    let dir = opts.keep_backup.as_deref()?;
    has_previous_backup(dir, udid).then(|| dir.join(udid))
}

/// Back up device `udid` (see [`pick_device`]) and return the path of its backup.
///
/// The temporary backup folder is removed on error or cancellation.
pub fn take_backup(
    opts: &DeviceOptions,
    udid: &str,
    cancel: &CancelToken,
    progress: ProgressFn<'_>,
) -> Result<BackupSource> {
    let program = tool(IDEVICEBACKUP2)?;
    let (root, temp) = if let Some(dir) = &opts.keep_backup {
        fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        (dir.clone(), None)
    } else {
        let mut builder = tempfile::Builder::new();
        builder.prefix("iwme-backup-");
        let td = opts
            .work_dir
            .as_ref()
            .map_or_else(|| builder.tempdir(), |w| builder.tempdir_in(w))
            .context("cannot create temporary backup folder")?;
        (td.path().to_owned(), Some(td))
    };
    let full = !has_previous_backup(&root, udid);
    let status = run_with_progress(&program, &backup_args(udid, &root, full), cancel, progress)?;
    if !status.success() {
        return Err(status.code().map_or_else(
            || anyhow!("{IDEVICEBACKUP2} was terminated by a signal"),
            |c| anyhow!("{IDEVICEBACKUP2} failed with exit code {c}"),
        ));
    }
    let path = root.join(udid);
    if !path.join("Manifest.plist").is_file() {
        bail!(
            "backup finished but {} has no Manifest.plist",
            path.display()
        );
    }
    Ok(BackupSource { path, _temp: temp })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn parses_device_list() {
        assert_eq!(parse_device_list("abc\n\n def \n"), s(&["abc", "def"]));
        assert_eq!(parse_device_list(""), Vec::<String>::new());
    }

    #[test]
    fn selects_device() {
        assert_eq!(select_device(&s(&["a"]), None).unwrap(), "a");
        assert_eq!(select_device(&s(&["a", "b"]), Some("b")).unwrap(), "b");
        assert!(
            select_device(&[], None)
                .unwrap_err()
                .to_string()
                .contains("no iPhone")
        );
        assert!(
            select_device(&s(&["a", "b"]), None)
                .unwrap_err()
                .to_string()
                .contains("--udid")
        );
        assert!(
            select_device(&s(&["a"]), Some("z"))
                .unwrap_err()
                .to_string()
                .contains("not connected")
        );
    }

    #[test]
    fn parses_percent() {
        assert_eq!(parse_percent("[=====>   ] 45% Finished"), Some(45.0));
        assert_eq!(parse_percent("x 12.5% then 99% done"), Some(99.0));
        assert_eq!(parse_percent("[====]7%"), Some(7.0));
        assert_eq!(parse_percent("no progress"), None);
        assert_eq!(parse_percent("150%"), None);
        assert_eq!(parse_percent("100% free"), Some(100.0));
    }

    #[test]
    fn splits_lines() {
        let mut sp = LineSplitter::default();
        assert_eq!(sp.push(b"one\ntw"), s(&["one"]));
        assert_eq!(sp.push(b"o\r\r10%\x1b[K\r"), s(&["two", "10%"]));
        assert_eq!(sp.push(b"tail"), Vec::<String>::new());
        assert_eq!(sp.finish().as_deref(), Some("tail"));
        assert_eq!(sp.finish(), None);
    }

    #[test]
    fn lists_backups_missing_and_present() {
        let t = tempfile::tempdir().unwrap();
        assert!(list_backups(&t.path().join("nope")).unwrap().is_empty());
        fake_backup(t.path(), "AAA");
        let found = list_backups(t.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert!(!found[0].encrypted);
        assert_eq!(found[0].device_name, None);
    }

    #[test]
    fn parses_will_encrypt() {
        assert_eq!(parse_will_encrypt("true\n"), Some(true));
        assert_eq!(parse_will_encrypt("false"), Some(false));
        assert_eq!(parse_will_encrypt("ERROR: no"), None);
        assert_eq!(parse_will_encrypt(""), None);
    }

    #[test]
    fn builds_backup_args() {
        let d = Path::new("/tmp/x");
        assert_eq!(
            backup_args("U", d, false),
            s(&["-u", "U", "backup", "/tmp/x"])
        );
        assert_eq!(
            backup_args("U", d, true),
            s(&["-u", "U", "backup", "--full", "/tmp/x"])
        );
    }

    fn fake_backup(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::create_dir_all(&p).unwrap();
        fs::write(p.join("Manifest.plist"), b"x").unwrap();
        p
    }

    #[test]
    fn finds_backups() {
        let t = tempfile::tempdir().unwrap();
        assert!(
            find_backup(t.path())
                .unwrap_err()
                .to_string()
                .contains("no iPhone backup")
        );
        let a = fake_backup(t.path(), "AAA");
        assert_eq!(find_backup(t.path()).unwrap(), a);
        assert_eq!(find_backup(&a).unwrap(), a);
        assert!(has_previous_backup(t.path(), "AAA"));
        assert!(!has_previous_backup(t.path(), "BBB"));
        fake_backup(t.path(), "BBB");
        let err = find_backup(t.path()).unwrap_err().to_string();
        assert!(
            err.contains("AAA") && err.contains("BBB") && err.contains("--backup"),
            "{err}"
        );
    }
}
