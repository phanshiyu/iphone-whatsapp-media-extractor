//! Pure, testable GUI state logic.

use iwme_core::{FullDiskAccessRequired, Progress, Summary};
use std::path::{Path, PathBuf};

/// Where the media comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceMode {
    /// Back up the connected iPhone first.
    #[default]
    Device,
    /// Use a backup that already exists on disk.
    Existing,
}

/// Everything that decides whether "Extract" is enabled.
#[derive(Debug, Clone, Copy, Default)]
#[expect(clippy::struct_excessive_bools, reason = "independent form facts")]
pub struct StartInputs {
    pub mode: SourceMode,
    pub device_selected: bool,
    pub backup_selected: bool,
    pub output_set: bool,
    pub needs_password: bool,
    pub password_set: bool,
    pub running: bool,
}

impl StartInputs {
    /// Whether the extraction may start.
    #[must_use]
    pub const fn can_start(self) -> bool {
        if self.running || !self.output_set {
            return false;
        }
        if self.needs_password && !self.password_set {
            return false;
        }
        match self.mode {
            SourceMode::Device => self.device_selected,
            SourceMode::Existing => self.backup_selected,
        }
    }
}

/// Whether a password field should be shown.
#[must_use]
pub const fn needs_password(
    mode: SourceMode,
    device_will_encrypt: Option<bool>,
    backup_encrypted: bool,
) -> bool {
    match mode {
        SourceMode::Device => matches!(device_will_encrypt, Some(true)),
        SourceMode::Existing => backup_encrypted,
    }
}

/// Default output folder: `~/Desktop/WhatsApp Media`.
#[must_use]
pub fn default_output_dir(home: Option<&Path>) -> PathBuf {
    home.map_or_else(
        || PathBuf::from("WhatsApp Media"),
        |h| h.join("Desktop").join("WhatsApp Media"),
    )
}

/// Short human text for the current phase.
#[must_use]
pub fn phase_text(p: &Progress) -> String {
    match p {
        Progress::BackingUp {
            percent: Some(pct), ..
        } => format!("Backing up your iPhone… {pct:.0}%"),
        Progress::BackingUp { .. } => "Backing up your iPhone…".to_owned(),
        Progress::Opening => "Reading the backup…".to_owned(),
        Progress::Extracting { done, total } => format!("Extracting files… {done} of {total}"),
        Progress::Warning(_) => "Working…".to_owned(),
    }
}

/// Progress bar fraction (0..=1), if known.
#[expect(clippy::cast_precision_loss, reason = "display only")]
#[must_use]
pub fn fraction(p: &Progress) -> Option<f32> {
    match p {
        Progress::BackingUp { percent, .. } => percent.map(|x| (x / 100.0).clamp(0.0, 1.0)),
        Progress::Extracting { done, total } if *total > 0 => {
            Some((*done as f32 / *total as f32).clamp(0.0, 1.0))
        }
        _ => None,
    }
}

/// A log line for a progress event, if it deserves one.
#[must_use]
pub fn log_line(p: &Progress) -> Option<String> {
    match p {
        Progress::BackingUp {
            percent: None,
            line,
        } => Some(line.clone()),
        Progress::Warning(w) => Some(format!("warning: {w}")),
        _ => None,
    }
}

/// Summary rows as (label, count).
#[must_use]
pub const fn summary_rows(s: &Summary) -> [(&'static str, usize); 6] {
    [
        ("Chats", s.chats),
        ("Extracted", s.extracted),
        ("Already present", s.up_to_date),
        ("Missing from backup", s.missing),
        ("Skipped (status)", s.skipped),
        ("Failed", s.failed),
    ]
}

/// A friendly one-paragraph message for an error.
#[must_use]
pub fn friendly_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    if text.contains("incorrect backup password") {
        "That password doesn't match this backup. Please check it and try again.".to_owned()
    } else if text.contains("no iPhone found") {
        "No iPhone found. Connect it with a cable, unlock it and tap \"Trust This Computer\", then press Refresh.".to_owned()
    } else {
        text
    }
}

/// Whether the error means macOS Full Disk Access is missing.
#[must_use]
pub fn is_full_disk_access(err: &anyhow::Error) -> bool {
    err.downcast_ref::<FullDiskAccessRequired>().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> StartInputs {
        StartInputs {
            mode: SourceMode::Device,
            device_selected: true,
            backup_selected: false,
            output_set: true,
            needs_password: false,
            password_set: false,
            running: false,
        }
    }

    #[test]
    fn can_start_rules() {
        assert!(ready().can_start());
        assert!(
            !StartInputs {
                running: true,
                ..ready()
            }
            .can_start()
        );
        assert!(
            !StartInputs {
                output_set: false,
                ..ready()
            }
            .can_start()
        );
        assert!(
            !StartInputs {
                device_selected: false,
                ..ready()
            }
            .can_start()
        );
        let existing = StartInputs {
            mode: SourceMode::Existing,
            ..ready()
        };
        assert!(!existing.can_start());
        assert!(
            StartInputs {
                backup_selected: true,
                ..existing
            }
            .can_start()
        );
        let pw = StartInputs {
            needs_password: true,
            ..ready()
        };
        assert!(!pw.can_start());
        assert!(
            StartInputs {
                password_set: true,
                ..pw
            }
            .can_start()
        );
    }

    #[test]
    fn password_visibility() {
        assert!(needs_password(SourceMode::Device, Some(true), false));
        assert!(!needs_password(SourceMode::Device, None, true));
        assert!(needs_password(SourceMode::Existing, None, true));
        assert!(!needs_password(SourceMode::Existing, Some(true), false));
    }

    #[test]
    fn default_output() {
        assert_eq!(
            default_output_dir(Some(Path::new("/Users/me"))),
            PathBuf::from("/Users/me/Desktop/WhatsApp Media")
        );
    }

    #[test]
    fn phases_and_fractions() {
        let b = Progress::BackingUp {
            percent: Some(45.4),
            line: "x".into(),
        };
        assert_eq!(phase_text(&b), "Backing up your iPhone… 45%");
        assert!((fraction(&b).unwrap() - 0.454).abs() < 1e-4);
        let e = Progress::Extracting { done: 1, total: 4 };
        assert_eq!(phase_text(&e), "Extracting files… 1 of 4");
        assert_eq!(fraction(&e), Some(0.25));
        assert_eq!(fraction(&Progress::Extracting { done: 0, total: 0 }), None);
        assert_eq!(fraction(&Progress::Opening), None);
        assert_eq!(log_line(&b), None);
        assert_eq!(
            log_line(&Progress::Warning("w".into())).as_deref(),
            Some("warning: w")
        );
    }

    #[test]
    fn friendly_errors() {
        let e = anyhow::anyhow!("incorrect backup password");
        assert!(friendly_error(&e).contains("password"));
        assert!(!is_full_disk_access(&e));
    }
}
