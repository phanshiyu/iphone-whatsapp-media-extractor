//! Extract WhatsApp media from an iPhone backup into per-chat folders.

pub mod backup;
pub mod chats;
pub mod crypto;
pub mod dedupe;
pub mod device;
pub mod error;
pub mod keybag;
pub mod output;
pub mod tools;
pub mod whatsapp;

use anyhow::{Context, Result, bail};
use backup::{Backup, FileEntry};
use chats::ChatInfo;
pub use dedupe::Dedupe;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use whatsapp::MediaRecord;

pub use error::{Cancelled, FullDiskAccessRequired};

/// A cloneable flag used to cancel long-running work from another thread.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// A fresh, not-yet-cancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Fail with [`Cancelled`] if cancellation was requested.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    }
}

/// A progress notification.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// The device backup is running; `line` is the latest tool output.
    BackingUp {
        /// Percentage parsed from the output, if any.
        percent: Option<f32>,
        /// The raw output line.
        line: String,
    },
    /// Reading the backup's file index and WhatsApp database.
    Opening,
    /// Copying media files: `done` of `total` handled so far.
    Extracting {
        /// Files handled so far.
        done: usize,
        /// Files to handle.
        total: usize,
    },
    /// Something went wrong that did not stop the run.
    Warning(String),
}

/// Receives [`Progress`] notifications.
pub type ProgressFn<'a> = &'a dyn Fn(Progress);

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
    /// What to do with files whose content is identical to an earlier one.
    /// In a dry run, duplicates are found by hashing the source files.
    pub dedupe: Dedupe,
    /// Chat keys (JIDs) to extract; `None` means all chats.
    pub chats: Option<HashSet<String>>,
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
    /// Skipped because identical to an earlier file (see [`Dedupe`]).
    pub duplicates: usize,
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "chats: {}, extracted: {}, already present: {}, missing from backup: {}, skipped (status): {}, duplicates: {}, failed: {}",
            self.chats,
            self.extracted,
            self.up_to_date,
            self.missing,
            self.skipped,
            self.duplicates,
            self.failed
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

/// An opened backup with its WhatsApp database read, ready to list chats and
/// extract media (the backup is opened and decrypted only once).
#[derive(Debug)]
pub struct Extractor {
    backup: Backup,
    files: HashMap<String, FileEntry>,
    records: Vec<MediaRecord>,
}

impl Extractor {
    /// Open the backup and read the media records. Uses `backup_dir`,
    /// `password` and `business` from `opts`.
    pub fn open(opts: &Options, cancel: &CancelToken, progress: ProgressFn<'_>) -> Result<Self> {
        cancel.check()?;
        progress(Progress::Opening);
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
        let records = whatsapp::load_media(&conn)?;
        Ok(Self {
            backup,
            files,
            records,
        })
    }

    fn visible(&self, include_status: bool) -> Vec<MediaRecord> {
        self.records
            .iter()
            .filter(|r| include_status || !is_status(r))
            .cloned()
            .collect()
    }

    /// Chats with media, most recently active first.
    #[must_use]
    pub fn list_chats(&self, include_status: bool) -> Vec<ChatInfo> {
        let records = self.visible(include_status);
        let dirs = output::chat_dirs(&records);
        let mut by_chat: HashMap<i64, ChatInfo> = HashMap::new();
        for r in &records {
            let unix = output::apple_to_unix(r.apple_date);
            let c = by_chat.entry(r.chat_pk).or_insert_with(|| ChatInfo {
                key: r.chat_jid.clone(),
                name: dirs.get(&r.chat_pk).cloned().unwrap_or_default(),
                is_group: r.is_group,
                media_count: 0,
                first: unix,
                last: unix,
            });
            c.media_count += 1;
            c.first = c.first.min(unix);
            c.last = c.last.max(unix);
        }
        let mut chats: Vec<ChatInfo> = by_chat.into_values().collect();
        chats.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| a.name.cmp(&b.name)));
        chats
    }

    /// Extract media (only `opts.chats`, if set).
    #[expect(clippy::too_many_lines, reason = "one linear extraction pipeline")]
    pub fn run(
        &self,
        opts: &Options,
        cancel: &CancelToken,
        progress: ProgressFn<'_>,
    ) -> Result<Summary> {
        let mut summary = Summary::default();
        let records = self.visible(opts.include_status);
        summary.skipped = self.records.len() - records.len();
        let exif = if opts.exif && !exiftool_ready(progress) {
            false
        } else {
            opts.exif
        };
        // Plan over all chats so folder names do not depend on the selection.
        let planned: Vec<_> = output::plan(&records, opts.utc)
            .into_iter()
            .filter(|p| {
                opts.chats.as_ref().is_none_or(|set| {
                    records
                        .get(p.record)
                        .is_some_and(|r| set.contains(&r.chat_jid))
                })
            })
            .collect();
        let total = planned.len();
        let mut index = dedupe::Index::new(opts.dedupe);
        let mut chat_dirs: HashSet<String> = HashSet::new();
        for (n, p) in planned.iter().enumerate() {
            cancel.check()?;
            progress(Progress::Extracting { done: n, total });
            let Some(rec) = records.get(p.record) else {
                continue;
            };
            let Some(entry) = resolve(&self.files, &rec.media_path) else {
                summary.missing += 1;
                continue;
            };
            let dir = opts.output_dir.join(&p.dir);
            let dest = dir.join(&p.name);
            let present = fs::metadata(&dest).ok().map(|m| m.len());
            let expected = entry.size.or_else(|| {
                if self.backup.encrypted() {
                    None
                } else {
                    self.backup.stored_len(entry)
                }
            });
            let up_to_date = match (present, expected) {
                (Some(have), Some(want)) => have == want,
                (Some(have), None) => have > 0,
                _ => false,
            };
            let warn = |what: &str, e: &dyn fmt::Display| {
                progress(Progress::Warning(format!("{}: {what}{e}", p.name)));
            };
            if up_to_date {
                // Register what is already on disk so dedupe stays consistent across runs.
                if index.enabled() {
                    match dedupe::hash_file(&dest) {
                        Ok(id) => {
                            index.insert(rec.chat_pk, id);
                        }
                        Err(e) => warn("cannot hash existing file: ", &e),
                    }
                }
            } else if opts.dry_run {
                if index.enabled() {
                    match self.backup.content_id(entry) {
                        Ok(id) if !index.insert(rec.chat_pk, id) => {
                            summary.duplicates += 1;
                            continue;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn("", &format_args!("{e:#}"));
                            summary.failed += 1;
                            continue;
                        }
                    }
                }
            } else {
                let res = fs::create_dir_all(&dir)
                    .map_err(anyhow::Error::from)
                    .and_then(|()| {
                        if index.enabled() {
                            self.backup.copy_file_unless_duplicate(entry, &dest, |id| {
                                index.insert(rec.chat_pk, id)
                            })
                        } else {
                            self.backup.copy_file_to(entry, &dest).map(Some)
                        }
                    });
                match res {
                    Err(e) => {
                        warn("", &format_args!("{e:#}"));
                        summary.failed += 1;
                        continue;
                    }
                    Ok(None) => {
                        summary.duplicates += 1;
                        continue;
                    }
                    Ok(Some(_)) => {}
                }
                if exif {
                    let ext = dest.extension().and_then(|e| e.to_str()).unwrap_or("");
                    let _ = output::write_exif_dates(&dest, ext, p.unix, opts.utc);
                }
            }
            if !opts.dry_run
                && let Err(e) = output::set_file_times(&dest, p.unix)
            {
                progress(Progress::Warning(format!(
                    "cannot set times on {}: {e}",
                    dest.display()
                )));
            }
            if up_to_date {
                summary.up_to_date += 1;
            } else {
                summary.extracted += 1;
            }
            chat_dirs.insert(p.dir.clone());
        }
        progress(Progress::Extracting { done: total, total });
        summary.chats = chat_dirs.len();
        Ok(summary)
    }
}

/// Open the backup and extract in one go.
pub fn run(opts: &Options, cancel: &CancelToken, progress: ProgressFn<'_>) -> Result<Summary> {
    Extractor::open(opts, cancel, progress)?.run(opts, cancel, progress)
}

fn exiftool_ready(progress: ProgressFn<'_>) -> bool {
    let ok = output::exiftool_available();
    if !ok {
        progress(Progress::Warning(
            "exiftool was not found; skipping EXIF tags".to_owned(),
        ));
    }
    ok
}
