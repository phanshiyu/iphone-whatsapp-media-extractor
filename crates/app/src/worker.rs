//! Background jobs; results come back over a channel.

use iwme_core::backup::Backup;
use iwme_core::chats::ChatInfo;
use iwme_core::device::{self, BackupInfo, BackupSource, Device, DeviceOptions};
use iwme_core::{CancelToken, Extractor, Options, Progress, Summary};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
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
    Loaded(Result<Session, anyhow::Error>),
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

/// Everything a load job needs.
#[derive(Debug)]
pub struct Job {
    pub source: Source,
    pub options: Options,
}

/// An opened backup, kept between loading chats and extracting. Field order
/// matters: the extractor (which has files open) drops before the temporary
/// backup folder is deleted.
#[derive(Debug)]
pub struct Session {
    pub extractor: Arc<Mutex<Extractor>>,
    pub chats: Vec<ChatInfo>,
    _source: BackupSource,
}

/// Back up the device if needed, open the backup and list its chats.
pub fn load(job: Job, cancel: CancelToken, tx: Sender<Msg>, ctx: egui::Context) {
    thread::spawn(move || {
        let progress = {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            move |p: Progress| send(&tx, &ctx, Msg::Progress(p))
        };
        let result = open_session(job, &cancel, &progress);
        send(&tx, &ctx, Msg::Loaded(result));
    });
}

fn open_session(
    job: Job,
    cancel: &CancelToken,
    progress: &dyn Fn(Progress),
) -> Result<Session, anyhow::Error> {
    let mut options = job.options;
    let source = match job.source {
        Source::Existing(path) => {
            if Backup::is_encrypted(&path)?
                && let Some(pw) = &options.password
            {
                Backup::verify_password(&path, pw)?;
            }
            BackupSource::existing(path)
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
    let extractor = Extractor::open(&options, cancel, progress)?;
    let chats = extractor.list_chats(options.include_status);
    Ok(Session {
        extractor: Arc::new(Mutex::new(extractor)),
        chats,
        _source: source,
    })
}

/// Extract the chats selected in `options` from an opened session.
pub fn extract(
    extractor: Arc<Mutex<Extractor>>,
    options: Options,
    cancel: CancelToken,
    tx: Sender<Msg>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let progress = {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            move |p: Progress| send(&tx, &ctx, Msg::Progress(p))
        };
        let result = extractor
            .lock()
            .map_err(|_| anyhow::anyhow!("an earlier extraction crashed"))
            .and_then(|ex| ex.run(&options, &cancel, &progress));
        send(&tx, &ctx, Msg::Done(result));
    });
}
