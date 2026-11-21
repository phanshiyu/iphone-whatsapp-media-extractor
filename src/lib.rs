//! Extract WhatsApp media from an iPhone backup into per-chat folders.

pub mod backup;
pub mod crypto;
pub mod device;
pub mod keybag;
pub mod output;
pub mod whatsapp;

use anyhow::{Context, Result, bail};
use backup::{Backup, FileEntry};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::PathBuf;
use whatsapp::MediaRecord;

/// WhatsApp (consumer) shared app-group domain.
pub const DOMAIN: &str = "AppDomainGroup-group.net.whatsapp.WhatsApp.shared";
/// WhatsApp Business shared app-group domain.
pub const BUSINESS_DOMAIN: &str = "AppDomainGroup-group.net.whatsapp.WhatsAppSMB.shared";

/// What to do.
#[derive(Debug, Clone, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "mirrors independent CLI flags"
)]
pub struct Options {
    /// Backup folder (contains `Manifest.plist`).
    pub backup_dir: PathBuf,
    /// Destination folder.
    pub output_dir: PathBuf,
    /// Backup password, if the backup is encrypted.
    pub password: Option<String>,
    /// Use the WhatsApp Business domain.
    pub business: bool,
    /// Also extract status updates.
    pub include_status: bool,
    /// Name files in UTC instead of local time.
    pub utc: bool,
    /// Report what would happen without writing anything.
    pub dry_run: bool,
    /// Also write date tags with exiftool.
    pub exif: bool,
}

/// Result counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Chats with at least one extracted or already present file.
    pub chats: usize,
    /// Files written (or that would be written in a dry run).
    pub extracted: usize,
    /// Files already present with the right size.
    pub up_to_date: usize,
    /// Referenced in the database but absent from the backup.
    pub missing: usize,
    /// Skipped (status updates).
    pub skipped: usize,
    /// Files that failed to copy.
    pub failed: usize,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "chats: {}, extracted: {}, already present: {}, missing from backup: {}, skipped (status): {}, failed: {}",
            self.chats, self.extracted, self.up_to_date, self.missing, self.skipped, self.failed
        )
    }
}

fn is_status(r: &MediaRecord) -> bool {
    r.chat_jid == "status@broadcast" || r.media_path.contains("@status")
}

/// Find the backup entry for a database media path.
fn resolve<'a>(files: &'a HashMap<String, FileEntry>, media_path: &str) -> Option<&'a FileEntry> {
    let p = media_path.trim_start_matches('/');
    let mut candidates = vec![format!("Message/{p}"), p.to_owned()];
    if let Some(i) = p.find("Media/") {
        candidates.push(format!("Message/{}", p.get(i..).unwrap_or(p)));
    }
    candidates.iter().find_map(|c| files.get(c))
}

/// Run the extraction.
pub fn run(opts: &Options) -> Result<Summary> {
    let backup = Backup::open(&opts.backup_dir, opts.password.as_deref())?;
    let domain = if opts.business {
        BUSINESS_DOMAIN
    } else {
        DOMAIN
    };
    let files = backup.files_in_domain(domain)?;
    if !files.contains_key("ChatStorage.sqlite") {
        bail!(
            "ChatStorage.sqlite not found in domain {domain}; wrong app (try --business) or no WhatsApp data in this backup"
        );
    }
    // Work on a scratch copy (with its WAL, if any) opened read-write so that
    // SQLite replays the write-ahead log.
    let scratch = tempfile::tempdir()?;
    for suffix in ["", "-wal", "-shm"] {
        let name = format!("ChatStorage.sqlite{suffix}");
        if let Some(entry) = files.get(&name) {
            backup
                .copy_file_to(entry, &scratch.path().join(&name))
                .with_context(|| format!("cannot extract {name}"))?;
        }
    }
    let conn = rusqlite::Connection::open_with_flags(
        scratch.path().join("ChatStorage.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .context("cannot open ChatStorage.sqlite")?;
    let all = whatsapp::load_media(&conn)?;

    let mut summary = Summary::default();
    let records: Vec<MediaRecord> = all
        .into_iter()
        .filter(|r| {
            let skip = !opts.include_status && is_status(r);
            summary.skipped += usize::from(skip);
            !skip
        })
        .collect();

    let exif = if opts.exif && !exiftool_ready() {
        false
    } else {
        opts.exif
    };
    let planned = output::plan(&records, opts.utc);
    let total = planned.len();
    let mut chat_dirs: HashSet<String> = HashSet::new();
    for (n, p) in planned.iter().enumerate() {
        let Some(rec) = records.get(p.record) else {
            continue;
        };
        let Some(entry) = resolve(&files, &rec.media_path) else {
            summary.missing += 1;
            continue;
        };
        let dir = opts.output_dir.join(&p.dir);
        let dest = dir.join(&p.name);
        let present = fs::metadata(&dest).ok().map(|m| m.len());
        let expected = entry.size.or_else(|| {
            if backup.encrypted() {
                None
            } else {
                backup.stored_len(entry)
            }
        });
        let up_to_date = match (present, expected) {
            (Some(have), Some(want)) => have == want,
            (Some(have), None) => have > 0,
            _ => false,
        };
        if !up_to_date && !opts.dry_run {
            let res = fs::create_dir_all(&dir)
                .map_err(anyhow::Error::from)
                .and_then(|()| backup.copy_file_to(entry, &dest));
            if let Err(e) = res {
                eprintln!("warning: {}: {e:#}", p.name);
                summary.failed += 1;
                continue;
            }
            if exif {
                let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or("");
                let _ = output::write_exif_dates(&dest, ext, p.unix, opts.utc);
            }
        }
        if !opts.dry_run {
            if let Err(e) = output::set_file_times(&dest, p.unix) {
                eprintln!("warning: cannot set times on {}: {e}", dest.display());
            }
        }
        if up_to_date {
            summary.up_to_date += 1;
        } else {
            summary.extracted += 1;
        }
        chat_dirs.insert(p.dir.clone());
        if (n + 1) % 200 == 0 {
            eprintln!("  {}/{total}", n + 1);
        }
    }
    summary.chats = chat_dirs.len();
    Ok(summary)
}

fn exiftool_ready() -> bool {
    let ok = output::exiftool_available();
    if !ok {
        eprintln!(
            "warning: --exif requested but `exiftool` was not found on PATH; skipping EXIF tags"
        );
    }
    ok
}
