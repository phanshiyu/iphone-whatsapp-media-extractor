//! Background jobs; results come back over a channel.

use iwme_core::backup::Backup;
use iwme_core::device::{self, BackupInfo, Device, DeviceOptions};
use iwme_core::{CancelToken, Options, Progress, Summary, run};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;

/// A connected device plus whether its backup will be encrypted.
#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub device: Device,
    pub will_encrypt: Option<bool>,
}

/// Messages sent to the UI thread.
#[derive(Debug)]
pub enum Msg {
    Devices(Result<Vec<DeviceEntry>, anyhow::Error>),
    Backups(Result<Vec<BackupInfo>, anyhow::Error>),
    Progress(Progress),
    Done(Result<Summary, anyhow::Error>),
}

/// Where a job reads from.
#[derive(Debug, Clone)]
pub enum Source {
    Device(String),
    Existing(PathBuf),
}

fn send(tx: &Sender<Msg>, ctx: &egui::Context, msg: Msg) {
    let _ = tx.send(msg);
    ctx.request_repaint();
}

pub fn refresh_devices(tx: Sender<Msg>, ctx: egui::Context) {
    thread::spawn(move || {
        let result = device::list_devices().map(|devices| {
            devices
                .into_iter()
                .map(|device| DeviceEntry {
                    will_encrypt: device::will_encrypt(&device.udid),
                    device,
                })
                .collect()
        });
        send(&tx, &ctx, Msg::Devices(result));
    });
}

pub fn refresh_backups(tx: Sender<Msg>, ctx: egui::Context) {
    thread::spawn(move || {
        let result = device::default_backup_root()
            .map_or_else(|| Ok(Vec::new()), |root| device::list_backups(&root));
        send(&tx, &ctx, Msg::Backups(result));
    });
}

/// Everything a job needs.
#[derive(Debug)]
pub struct Job {
    pub source: Source,
    pub options: Options,
}

pub fn start(job: Job, cancel: CancelToken, tx: Sender<Msg>, ctx: egui::Context) {
    thread::spawn(move || {
        let progress = {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            move |p: Progress| send(&tx, &ctx, Msg::Progress(p))
        };
        let result = execute(job, &cancel, &progress);
        send(&tx, &ctx, Msg::Done(result));
    });
}

fn execute(
    job: Job,
    cancel: &CancelToken,
    progress: &dyn Fn(Progress),
) -> Result<Summary, anyhow::Error> {
    let mut options = job.options;
    // Keep the source alive (and the temp backup on disk) until the end.
    let source = match job.source {
        Source::Existing(path) => {
            if Backup::is_encrypted(&path)?
                && let Some(pw) = &options.password
            {
                Backup::verify_password(&path, pw)?;
            }
            device::BackupSource::existing(path)
        }
        Source::Device(udid) => {
            let dev = DeviceOptions {
                udid: Some(udid.clone()),
                ..DeviceOptions::default()
            };
            device::take_backup(&dev, &udid, cancel, progress)?
        }
    };
    options.backup_dir.clone_from(&source.path);
    run(&options, cancel, progress)
}
