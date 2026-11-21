//! Command-line entry point.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use iwme_core::backup::Backup;
use iwme_core::chats::{ChatInfo, resolve_patterns};
use iwme_core::device::{self, BackupSource, DeviceOptions};
use iwme_core::error::{self, FullDiskAccessRequired};
use iwme_core::output::format_time;
use iwme_core::{CancelToken, Dedupe, Extractor, Options, Progress};
use std::cell::Cell;
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// How to treat files with identical content.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum DedupeArg {
    /// Keep every file
    Off,
    /// Skip files identical to an earlier one in the same chat
    PerChat,
    /// Skip files identical to an earlier one in any chat
    AllChats,
}

impl From<DedupeArg> for Dedupe {
    fn from(d: DedupeArg) -> Self {
        match d {
            DedupeArg::Off => Self::Off,
            DedupeArg::PerChat => Self::PerChat,
            DedupeArg::AllChats => Self::AllChats,
        }
    }
}

/// Extract WhatsApp media from an iPhone into one folder per chat.
///
/// Without --backup, the connected iPhone is backed up first (needs
/// libimobiledevice) and the backup is deleted afterwards.
#[derive(Debug, Parser)]
#[command(version, about)]
#[expect(clippy::struct_excessive_bools, reason = "independent CLI flags")]
struct Cli {
    /// Where to write the per-chat folders
    #[arg(required_unless_present = "list_chats")]
    output_dir: Option<PathBuf>,
    /// Use an existing backup (the UDID folder, or a folder holding one backup)
    #[arg(long, value_name = "BACKUP_DIR")]
    backup: Option<PathBuf>,
    /// Keep the device backup here (re-runs are incremental)
    #[arg(long, value_name = "DIR", conflicts_with_all = ["backup", "work_dir"])]
    keep_backup: Option<PathBuf>,
    /// Device to back up when several are connected
    #[arg(long, conflicts_with = "backup")]
    udid: Option<String>,
    /// Put the temporary backup here instead of the system temp dir
    #[arg(long, value_name = "DIR", conflicts_with = "backup")]
    work_dir: Option<PathBuf>,
    /// Backup password (prompted for if needed and not given)
    #[arg(long, env = "IWME_PASSWORD", hide_env_values = true)]
    password: Option<String>,
    /// Use the WhatsApp Business app instead of WhatsApp
    #[arg(long)]
    business: bool,
    /// Also extract status updates
    #[arg(long)]
    include_status: bool,
    /// Name files using UTC instead of local time
    #[arg(long)]
    utc: bool,
    /// Show what would be done without writing files
    #[arg(long)]
    dry_run: bool,
    /// Also write date tags with exiftool, if installed
    #[arg(long)]
    exif: bool,
    /// Skip files whose content is identical to an earlier one
    #[arg(long, value_enum, default_value = "off")]
    dedupe: DedupeArg,
    /// List the chats that have media, then exit
    #[arg(long)]
    list_chats: bool,
    /// Only extract this chat (JID or name; repeatable)
    #[arg(long = "chat", value_name = "NAME_OR_JID")]
    chat: Vec<String>,
    /// Do not extract this chat (JID or name; repeatable)
    #[arg(long = "exclude-chat", value_name = "NAME_OR_JID")]
    exclude_chat: Vec<String>,
}

fn print_chats(chats: &[ChatInfo]) {
    let width = chats
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0);
    for c in chats {
        println!(
            "{:<width$}  {:<6}  {:>6}  {}  {}",
            c.name,
            if c.is_group { "group" } else { "direct" },
            c.media_count,
            format_time(c.last, false, "%Y-%m-%d"),
            c.key,
        );
    }
}

/// The chat keys to extract, or `None` for all.
fn selection(cli: &Cli, chats: &[ChatInfo]) -> Result<Option<HashSet<String>>> {
    if cli.chat.is_empty() && cli.exclude_chat.is_empty() {
        return Ok(None);
    }
    let mut keys = if cli.chat.is_empty() {
        chats.iter().map(|c| c.key.clone()).collect()
    } else {
        resolve_patterns(chats, &cli.chat)?
    };
    for k in resolve_patterns(chats, &cli.exclude_chat)? {
        keys.remove(&k);
    }
    anyhow::ensure!(!keys.is_empty(), "no chats left to extract");
    Ok(Some(keys))
}

fn prompt_password() -> Result<String> {
    rpassword::prompt_password("Backup password: ").context("cannot read password")
}

/// Decide, before any long backup starts, whether a password is needed, and
/// check it against an existing kept backup so a typo fails fast.
fn password_for_device(cli: &Cli, opts: &DeviceOptions, udid: &str) -> Result<Option<String>> {
    let mut password = cli.password.clone();
    if let Some(existing) = device::existing_kept_backup(opts, udid) {
        if Backup::is_encrypted(&existing)? {
            let pw = match password {
                Some(pw) => pw,
                None => prompt_password()?,
            };
            Backup::verify_password(&existing, &pw)?;
            password = Some(pw);
        }
    } else if password.is_none() && device::will_encrypt(udid) == Some(true) {
        password = Some(prompt_password()?);
    }
    Ok(password)
}

/// Print progress to stderr: progress bars redraw in place, everything else
/// gets its own line, and extraction progress is reported every 200 files.
fn print_progress(redraw_pending: &Cell<bool>, p: &Progress) {
    let mut err = std::io::stderr().lock();
    let end_redraw = |err: &mut std::io::StderrLock<'_>| {
        if redraw_pending.replace(false) {
            let _ = writeln!(err);
        }
    };
    match p {
        Progress::BackingUp {
            percent: Some(_),
            line,
        } => {
            let _ = write!(err, "\r{line}");
            let _ = err.flush();
            redraw_pending.set(true);
        }
        Progress::BackingUp { line, .. } => {
            end_redraw(&mut err);
            let _ = writeln!(err, "{line}");
        }
        Progress::Extracting { done, total } if *done > 0 && done % 200 == 0 => {
            end_redraw(&mut err);
            let _ = writeln!(err, "  {done}/{total}");
        }
        Progress::Warning(w) => {
            end_redraw(&mut err);
            let _ = writeln!(err, "warning: {w}");
        }
        Progress::Extracting { .. } | Progress::Opening => {}
    }
}

fn real_main(cancel: &CancelToken) -> Result<()> {
    let cli = Cli::parse();
    let pending = Cell::new(false);
    let progress = |p: Progress| print_progress(&pending, &p);
    let (source, mut password) = if let Some(path) = &cli.backup {
        (
            BackupSource::existing(device::find_backup(path)?),
            cli.password.clone(),
        )
    } else {
        let opts = DeviceOptions {
            udid: cli.udid.clone(),
            keep_backup: cli.keep_backup.clone(),
            work_dir: cli.work_dir.clone(),
        };
        let udid = device::pick_device(&opts)?;
        let password = password_for_device(&cli, &opts, &udid)?;
        eprintln!(
            "Backing up device {udid}.\n\
             Unlock the iPhone and tap \"Trust\" / enter its passcode if prompted. The whole device \
             is backed up, which needs free disk space roughly equal to the used storage on the phone."
        );
        (
            device::take_backup(&opts, &udid, cancel, &progress)?,
            password,
        )
    };
    if password.is_none() && Backup::is_encrypted(&source.path)? {
        password = Some(prompt_password()?);
    }
    let mut opts = Options {
        backup_dir: source.path,
        output_dir: cli.output_dir.clone().unwrap_or_default(),
        password,
        business: cli.business,
        include_status: cli.include_status,
        utc: cli.utc,
        dry_run: cli.dry_run,
        exif: cli.exif,
        dedupe: cli.dedupe.into(),
        chats: None,
    };
    let extractor = Extractor::open(&opts, cancel, &progress)?;
    let chats = extractor.list_chats(cli.include_status);
    if cli.list_chats {
        print_chats(&chats);
        return Ok(());
    }
    opts.chats = selection(&cli, &chats)?;
    let summary = extractor.run(&opts, cancel, &progress)?;
    println!("{summary}{}", if cli.dry_run { " (dry run)" } else { "" });
    Ok(())
}

fn main() -> ExitCode {
    let cancel = CancelToken::new();
    let handler_token = cancel.clone();
    if let Err(e) = ctrlc::set_handler(move || handler_token.cancel()) {
        eprintln!("warning: cannot install Ctrl-C handler: {e}");
    }
    match real_main(&cancel) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) if error::is_cancelled(&err) => {
            eprintln!("\ninterrupted");
            ExitCode::from(130)
        }
        Err(err) => {
            eprintln!("Error: {err:?}");
            if err.downcast_ref::<FullDiskAccessRequired>().is_some()
                && std::io::stderr().is_terminal()
                && error::open_full_disk_access_settings()
            {
                eprintln!("I opened the Full Disk Access settings pane for you.");
            }
            ExitCode::FAILURE
        }
    }
}
