//! Typed errors the UI can inspect via `anyhow::Error::downcast_ref`.

use std::fmt;
use std::io;

/// macOS denied access to a protected folder (iPhone backups need Full Disk
/// Access for the app that is running this code).
#[derive(Debug)]
pub struct FullDiskAccessRequired {
    what: String,
    source: io::Error,
}

impl FullDiskAccessRequired {
    /// The `System Settings` URL of the Full Disk Access pane.
    pub const SETTINGS_URL: &'static str =
        "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";
}

impl fmt::Display for FullDiskAccessRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}\nmacOS blocks access to iPhone backups by default: give your terminal app Full \
             Disk Access in System Settings \u{2192} Privacy & Security \u{2192} Full Disk Access, \
             then quit and reopen the terminal. (Device mode, i.e. running without --backup, does \
             not need Full Disk Access because it writes its own backup to a temp folder.)",
            self.what, self.source
        )
    }
}

impl std::error::Error for FullDiskAccessRequired {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// The operation was cancelled through a [`crate::CancelToken`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// Whether `err` is (or wraps) a cancellation.
#[must_use]
pub fn is_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Cancelled>().is_some()
}

/// Open the macOS Full Disk Access settings pane (best effort).
#[must_use]
pub fn open_full_disk_access_settings() -> bool {
    std::process::Command::new("open")
        .arg(FullDiskAccessRequired::SETTINGS_URL)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Turn an I/O error into a [`FullDiskAccessRequired`] on macOS permission
/// errors, and a plain contextual error otherwise.
pub(crate) fn access_error(err: io::Error, what: &str) -> anyhow::Error {
    if err.kind() == io::ErrorKind::PermissionDenied && cfg!(target_os = "macos") {
        FullDiskAccessRequired {
            what: what.to_owned(),
            source: err,
        }
        .into()
    } else {
        anyhow::anyhow!("{what}: {err}")
    }
}
