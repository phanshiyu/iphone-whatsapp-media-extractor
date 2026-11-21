//! Taking a backup from a connected iPhone via libimobiledevice, and locating
//! existing backups.

use anyhow::{Context, Result, anyhow, bail};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;
use tempfile::TempDir;

/// Program that lists connected devices.
pub const IDEVICE_ID: &str = "idevice_id";
/// Program that reads device properties.
pub const IDEVICEINFO: &str = "ideviceinfo";
/// Program that takes the backup.
pub const IDEVICEBACKUP2: &str = "idevicebackup2";

const INSTALL_HINT: &str = "install libimobiledevice (macOS: `brew install libimobiledevice`; Debian/Ubuntu: `apt install libimobiledevice-utils`)";

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static CHILD_RUNNING: AtomicBool = AtomicBool::new(false);
static CLEANUP_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

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
        .with_context(|| format!("cannot read {}", path.display()))?
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

fn tool_exists(program: &str) -> bool {
    match Command::new(program)
        .arg("-h")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Err(e) => e.kind() != ErrorKind::NotFound,
        Ok(_) => true,
    }
}

fn require_tools() -> Result<()> {
    for prog in [IDEVICE_ID, IDEVICEINFO, IDEVICEBACKUP2] {
        if !tool_exists(prog) {
            bail!("`{prog}` was not found on PATH; {INSTALL_HINT}");
        }
    }
    Ok(())
}

/// Wait for the child, killing it if Ctrl-C was pressed.
fn wait_interruptible(child: &mut Child) -> Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if INTERRUPTED.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("interrupted");
        }
        sleep(Duration::from_millis(200));
    }
}

fn install_ctrlc_handler() {
    let result = ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
        if !CHILD_RUNNING.load(Ordering::SeqCst) {
            // Not waiting on the backup, so nobody else will clean up.
            if let Ok(guard) = CLEANUP_DIR.lock() {
                if let Some(dir) = guard.as_ref() {
                    let _ = fs::remove_dir_all(dir);
                }
            }
            std::process::exit(130);
        }
    });
    if let Err(e) = result {
        eprintln!("warning: cannot install Ctrl-C handler: {e}");
    }
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
    let out = Command::new(IDEVICEINFO)
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
    let listing = Command::new(IDEVICE_ID)
        .arg("-l")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("cannot run {IDEVICE_ID}"))?;
    select_device(
        &parse_device_list(&String::from_utf8_lossy(&listing.stdout)),
        opts.udid.as_deref(),
    )
}

/// The persistent backup folder for `udid`, if `--keep-backup` points at an existing one.
#[must_use]
pub fn existing_kept_backup(opts: &DeviceOptions, udid: &str) -> Option<PathBuf> {
    let dir = opts.keep_backup.as_deref()?;
    has_previous_backup(dir, udid).then(|| dir.join(udid))
}

/// Back up device `udid` (see [`pick_device`]) and return the path of its backup.
pub fn take_backup(opts: &DeviceOptions, udid: &str) -> Result<BackupSource> {
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
        if let Ok(mut g) = CLEANUP_DIR.lock() {
            *g = Some(td.path().to_owned());
        }
        (td.path().to_owned(), Some(td))
    };
    install_ctrlc_handler();

    eprintln!(
        "Backing up device {udid} to {}.\n\
         Unlock the iPhone and tap \"Trust\" / enter its passcode if prompted. The whole device \
         is backed up, which needs free disk space roughly equal to the used storage on the phone.",
        root.display()
    );
    let full = !has_previous_backup(&root, udid);
    CHILD_RUNNING.store(true, Ordering::SeqCst);
    let status = Command::new(IDEVICEBACKUP2)
        .args(backup_args(udid, &root, full))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("cannot run {IDEVICEBACKUP2}"))
        .and_then(|mut child| wait_interruptible(&mut child));
    CHILD_RUNNING.store(false, Ordering::SeqCst);
    let status = status?;
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
