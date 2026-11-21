//! Command-line entry point.

use anyhow::{Context, Result};
use clap::Parser;
use iphone_whatsapp_media_extractor::backup::Backup;
use iphone_whatsapp_media_extractor::device::{self, BackupSource, DeviceOptions};
use iphone_whatsapp_media_extractor::{Options, run};
use std::path::PathBuf;

/// Extract WhatsApp media from an iPhone into one folder per chat.
///
/// Without --backup, the connected iPhone is backed up first (needs
/// libimobiledevice) and the backup is deleted afterwards.
#[derive(Debug, Parser)]
#[command(version, about)]
#[expect(clippy::struct_excessive_bools, reason = "independent CLI flags")]
struct Cli {
    /// Where to write the per-chat folders
    output_dir: PathBuf,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
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
        (device::take_backup(&opts, &udid)?, password)
    };
    if password.is_none() && Backup::is_encrypted(&source.path)? {
        password = Some(prompt_password()?);
    }
    let summary = run(&Options {
        backup_dir: source.path,
        output_dir: cli.output_dir,
        password,
        business: cli.business,
        include_status: cli.include_status,
        utc: cli.utc,
        dry_run: cli.dry_run,
        exif: cli.exif,
    })?;
    println!("{summary}{}", if cli.dry_run { " (dry run)" } else { "" });
    Ok(())
}
