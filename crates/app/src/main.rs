//! Simple desktop app: extract WhatsApp media from an iPhone into per-chat folders.
#![expect(missing_docs, reason = "internal modules of a binary crate")]

pub mod logic;
pub mod worker;

use eframe::egui;
use iwme_core::device::{self, BackupInfo};
use iwme_core::error::{self, is_cancelled};
use iwme_core::output::format_time;
use iwme_core::tools::{find_tool, home_dir};
use iwme_core::{CancelToken, Dedupe, Options, Progress, Summary};
use logic::{SourceMode, StartInputs};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};
use worker::{DeviceEntry, Job, Msg, Session, Source};

const TITLE: &str = "iPhone WhatsApp Media Extractor";
const BREW_CMD: &str = "brew install libimobiledevice";
const LOG_LIMIT: usize = 300;

#[expect(clippy::struct_excessive_bools, reason = "independent form flags")]
struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    mode: SourceMode,
    missing_tools: Vec<&'static str>,
    devices: Vec<DeviceEntry>,
    devices_loading: bool,
    selected_device: usize,
    backups: Vec<BackupInfo>,
    backups_loading: bool,
    selected_backup: Option<usize>,
    fda_needed: bool,
    output: String,
    password: String,
    business: bool,
    include_status: bool,
    utc: bool,
    exif: bool,
    exiftool_ok: bool,
    dedupe: Dedupe,
    session: Option<Session>,
    checked: HashSet<String>,
    filter: String,
    running: bool,
    cancel: CancelToken,
    phase: String,
    fraction: Option<f32>,
    log: Vec<String>,
    summary: Option<Summary>,
    output_done: Option<PathBuf>,
    error: Option<String>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        egui_extras::install_image_loaders(ctx);
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            tx,
            rx,
            mode: SourceMode::Device,
            missing_tools: device::missing_tools(),
            devices: Vec::new(),
            devices_loading: false,
            selected_device: 0,
            backups: Vec::new(),
            backups_loading: false,
            selected_backup: None,
            fda_needed: false,
            output: logic::default_output_dir(home_dir().as_deref())
                .display()
                .to_string(),
            password: String::new(),
            business: false,
            include_status: false,
            utc: false,
            exif: false,
            exiftool_ok: find_tool("exiftool").is_some(),
            dedupe: Dedupe::Off,
            session: None,
            checked: HashSet::new(),
            filter: String::new(),
            running: false,
            cancel: CancelToken::new(),
            phase: String::new(),
            fraction: None,
            log: Vec::new(),
            summary: None,
            output_done: None,
            error: None,
        };
        app.refresh_devices(ctx);
        app.refresh_backups(ctx);
        app
    }

    fn refresh_devices(&mut self, ctx: &egui::Context) {
        self.missing_tools = device::missing_tools();
        if self.missing_tools.is_empty() {
            self.devices_loading = true;
            worker::refresh_devices(self.tx.clone(), ctx.clone());
        } else {
            self.devices.clear();
        }
    }

    fn refresh_backups(&mut self, ctx: &egui::Context) {
        self.backups_loading = true;
        worker::refresh_backups(self.tx.clone(), ctx.clone());
    }

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Devices(r) => {
                    self.devices_loading = false;
                    match r {
                        Ok(d) => {
                            self.devices = d;
                            self.selected_device = 0;
                        }
                        Err(e) => {
                            self.devices.clear();
                            self.error = Some(logic::friendly_error(&e));
                        }
                    }
                }
                Msg::Backups(r) => {
                    self.backups_loading = false;
                    match r {
                        Ok(b) => {
                            self.fda_needed = false;
                            self.backups = b;
                            self.selected_backup = (self.backups.len() == 1).then_some(0);
                        }
                        Err(e) => {
                            self.fda_needed = logic::is_full_disk_access(&e);
                            if !self.fda_needed {
                                self.error = Some(logic::friendly_error(&e));
                            }
                        }
                    }
                }
                Msg::Progress(p) => {
                    self.phase = logic::phase_text(&p);
                    if let Some(f) = logic::fraction(&p) {
                        self.fraction = Some(f);
                    } else if !matches!(p, Progress::Warning(_)) {
                        self.fraction = None;
                    }
                    if let Some(line) = logic::log_line(&p) {
                        self.log.push(line);
                        if self.log.len() > LOG_LIMIT {
                            self.log.remove(0);
                        }
                    }
                }
                Msg::Loaded(r) => {
                    self.running = false;
                    match r {
                        Ok(session) => {
                            self.checked = session.chats.iter().map(|c| c.key.clone()).collect();
                            self.filter.clear();
                            self.session = Some(session);
                        }
                        Err(e) => self.fail(&e),
                    }
                }
                Msg::Done(r) => {
                    self.running = false;
                    match r {
                        Ok(s) => {
                            self.summary = Some(s);
                            self.output_done = Some(PathBuf::from(&self.output));
                            // The temporary device backup is no longer needed.
                            if self.mode == SourceMode::Device {
                                self.session = None;
                            }
                        }
                        Err(e) => self.fail(&e),
                    }
                }
            }
        }
    }

    fn fail(&mut self, e: &anyhow::Error) {
        if is_cancelled(e) {
            self.error = Some("Cancelled.".to_owned());
        } else {
            self.fda_needed = logic::is_full_disk_access(e);
            self.error = Some(logic::friendly_error(e));
        }
    }

    fn device_encrypts(&self) -> Option<bool> {
        self.devices
            .get(self.selected_device)
            .and_then(|d| d.will_encrypt)
    }

    fn backup_encrypted(&self) -> bool {
        self.selected_backup
            .and_then(|i| self.backups.get(i))
            .is_some_and(|b| b.encrypted)
    }

    fn needs_password(&self) -> bool {
        logic::needs_password(self.mode, self.device_encrypts(), self.backup_encrypted())
    }

    fn start_inputs(&self) -> StartInputs {
        StartInputs {
            mode: self.mode,
            device_selected: self.selected_device < self.devices.len(),
            backup_selected: self.selected_backup.is_some_and(|i| i < self.backups.len()),
            needs_password: self.needs_password(),
            password_set: !self.password.is_empty(),
            running: self.running,
        }
    }

    fn reset_run(&mut self, phase: &str) {
        self.cancel = CancelToken::new();
        self.running = true;
        self.error = None;
        self.summary = None;
        self.fda_needed = false;
        self.log.clear();
        self.fraction = None;
        phase.clone_into(&mut self.phase);
    }

    fn options(&self) -> Options {
        Options {
            backup_dir: PathBuf::new(),
            output_dir: PathBuf::from(self.output.trim()),
            password: self.needs_password().then(|| self.password.clone()),
            business: self.business,
            include_status: self.include_status,
            utc: self.utc,
            dry_run: false,
            exif: self.exif && self.exiftool_ok,
            dedupe: self.dedupe,
            chats: None,
        }
    }

    /// Step 1: back up the device if needed, open the backup, list the chats.
    fn load_chats(&mut self, ctx: &egui::Context) {
        let source = match self.mode {
            SourceMode::Device => self
                .devices
                .get(self.selected_device)
                .map(|d| Source::Device(d.device.udid.clone())),
            SourceMode::Existing => self
                .selected_backup
                .and_then(|i| self.backups.get(i))
                .map(|b| Source::Existing(b.path.clone())),
        };
        let Some(source) = source else { return };
        let job = Job {
            source,
            options: self.options(),
        };
        self.reset_run("Starting…");
        worker::load(job, self.cancel.clone(), self.tx.clone(), ctx.clone());
    }

    /// Step 2: extract the checked chats.
    fn extract(&mut self, ctx: &egui::Context) {
        let Some(session) = &self.session else { return };
        let extractor = Arc::clone(&session.extractor);
        let options = Options {
            chats: Some(self.checked.clone()),
            ..self.options()
        };
        self.reset_run("Starting…");
        worker::extract(
            extractor,
            options,
            self.cancel.clone(),
            self.tx.clone(),
            ctx.clone(),
        );
    }

    fn header(ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add(
                egui::Image::new(egui::include_image!("../../../assets/logo.png"))
                    .fit_to_exact_size(egui::vec2(56.0, 56.0)),
            );
            ui.heading(TITLE);
        });
    }

    fn source_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Source").strong());
        ui.radio_value(
            &mut self.mode,
            SourceMode::Device,
            "Back up my connected iPhone",
        );
        if self.mode == SourceMode::Device {
            ui.indent("device", |ui| self.device_ui(ui));
        }
        ui.radio_value(
            &mut self.mode,
            SourceMode::Existing,
            "Use an existing backup",
        );
        if self.mode == SourceMode::Existing {
            ui.indent("existing", |ui| self.existing_ui(ui));
        }
    }

    fn device_ui(&mut self, ui: &mut egui::Ui) {
        if !self.missing_tools.is_empty() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "libimobiledevice is not installed. Install with:",
            );
            ui.horizontal(|ui| {
                ui.code(BREW_CMD);
                if ui.button("Copy").clicked() {
                    ui.ctx().copy_text(BREW_CMD.to_owned());
                }
            });
        } else if self.devices_loading {
            ui.label("Looking for your iPhone…");
        } else if self.devices.is_empty() {
            ui.label("No iPhone found. Connect it, unlock it and tap \"Trust\".");
        } else if let [only] = self.devices.as_slice() {
            ui.label(format!("Found: {}", only.device.label()));
        } else {
            let current = self
                .devices
                .get(self.selected_device)
                .map_or_else(String::new, |d| d.device.label().to_owned());
            egui::ComboBox::from_id_salt("device")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for (i, d) in self.devices.iter().enumerate() {
                        ui.selectable_value(&mut self.selected_device, i, d.device.label());
                    }
                });
        }
        if ui.button("Refresh").clicked() {
            let ctx = ui.ctx().clone();
            self.refresh_devices(&ctx);
        }
    }

    fn existing_ui(&mut self, ui: &mut egui::Ui) {
        if self.fda_needed {
            Self::fda_banner(ui);
        } else if self.backups_loading {
            ui.label("Looking for backups…");
        } else if self.backups.is_empty() {
            ui.label("No backups found in the default folder.");
        }
        for (i, b) in self.backups.iter().enumerate() {
            let when = b.last_backup.map_or_else(String::new, |t| {
                t.to_zoned_datetime(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M")
                    .to_string()
            });
            let lock = if b.encrypted { " \u{1F512}" } else { "" };
            let name = b.device_name.as_deref().unwrap_or("Unknown device");
            ui.radio_value(
                &mut self.selected_backup,
                Some(i),
                format!("{name}  {when}{lock}"),
            );
        }
        ui.horizontal(|ui| {
            if ui.button("Choose folder…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                match device::find_backup(&dir) {
                    Ok(path) => {
                        self.backups.push(device::backup_info(&path));
                        self.selected_backup = Some(self.backups.len() - 1);
                    }
                    Err(e) => {
                        self.fda_needed = logic::is_full_disk_access(&e);
                        self.error = Some(logic::friendly_error(&e));
                    }
                }
            }
            if ui.button("Refresh").clicked() {
                let ctx = ui.ctx().clone();
                self.refresh_backups(&ctx);
            }
        });
    }

    fn fda_banner(ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "macOS needs your permission to read iPhone backups.",
            );
            ui.label(
                "Open Settings, turn on Full Disk Access for this app, then quit and reopen it.",
            );
            if ui.button("Open Settings").clicked() {
                let _ = error::open_full_disk_access_settings();
            }
        });
    }

    fn password_section(&mut self, ui: &mut egui::Ui) {
        if self.needs_password() {
            ui.label(egui::RichText::new("Backup password").strong());
            ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .password(true)
                    .desired_width(300.0),
            );
        }
    }

    fn options_section(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Save to").strong());
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.output).desired_width(300.0));
            if ui.button("Choose…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.output = dir.display().to_string();
            }
        });
        ui.collapsing("More options", |ui| {
            ui.add_enabled_ui(self.session.is_none(), |ui| {
                ui.checkbox(&mut self.business, "WhatsApp Business");
                ui.checkbox(&mut self.include_status, "Include status updates");
            });
            ui.checkbox(&mut self.utc, "Use UTC in file names");
            ui.horizontal(|ui| {
                ui.label("Duplicates:");
                egui::ComboBox::from_id_salt("dedupe")
                    .selected_text(logic::dedupe_label(self.dedupe))
                    .show_ui(ui, |ui| {
                        for d in [Dedupe::Off, Dedupe::PerChat, Dedupe::AllChats] {
                            ui.selectable_value(&mut self.dedupe, d, logic::dedupe_label(d));
                        }
                    });
            });
            ui.add_enabled(
                self.exiftool_ok,
                egui::Checkbox::new(&mut self.exif, "Write EXIF dates"),
            )
            .on_disabled_hover_text("Needs exiftool: brew install exiftool");
        });
    }

    fn chats_ui(&mut self, ui: &mut egui::Ui) {
        let Some(session) = &self.session else { return };
        let mut change_source = false;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Chats").strong());
            ui.label(format!(
                "{} of {} selected",
                self.checked.len(),
                session.chats.len()
            ));
            change_source = ui.button("Change source").clicked();
        });
        let visible = logic::filter_chats(&session.chats, &self.filter);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text("Search")
                    .desired_width(160.0),
            );
            if ui.button("Select all").clicked() {
                self.checked.extend(visible.iter().map(|c| c.key.clone()));
            }
            if ui.button("Select none").clicked() {
                for c in &visible {
                    self.checked.remove(&c.key);
                }
            }
        });
        let row = ui.spacing().interact_size.y;
        egui::ScrollArea::vertical()
            .id_salt("chats")
            .max_height(260.0)
            .auto_shrink([false, true])
            .show_rows(ui, row, visible.len(), |ui, range| {
                for c in visible.get(range).unwrap_or_default() {
                    ui.horizontal(|ui| {
                        let mut on = self.checked.contains(&c.key);
                        if ui.checkbox(&mut on, &c.name).changed() {
                            if on {
                                self.checked.insert(c.key.clone());
                            } else {
                                self.checked.remove(&c.key);
                            }
                        }
                        let kind = if c.is_group { "group \u{b7} " } else { "" };
                        ui.weak(format!(
                            "{kind}{} media \u{b7} {}",
                            c.media_count,
                            format_time(c.last, false, "%Y-%m-%d")
                        ));
                    });
                }
            });
        let ready = !self.output.trim().is_empty() && !self.checked.is_empty();
        let go = egui::Button::new(egui::RichText::new("Extract").size(18.0))
            .min_size(egui::vec2(ui.available_width(), 40.0));
        if ui.add_enabled(ready, go).clicked() {
            let ctx = ui.ctx().clone();
            self.extract(&ctx);
        }
        if change_source {
            self.session = None;
        }
    }

    fn run_section(&mut self, ui: &mut egui::Ui) {
        if self.running {
            ui.label(&self.phase);
            let bar = self.fraction.map_or_else(
                || egui::ProgressBar::new(0.0).animate(true),
                egui::ProgressBar::new,
            );
            ui.add(bar);
            if ui.button("Cancel").clicked() {
                self.cancel.cancel();
                "Cancelling…".clone_into(&mut self.phase);
            }
        } else if self.session.is_some() {
            self.chats_ui(ui);
        } else {
            let go = egui::Button::new(egui::RichText::new("Load chats").size(18.0))
                .min_size(egui::vec2(ui.available_width(), 40.0));
            if ui.add_enabled(self.start_inputs().can_load(), go).clicked() {
                let ctx = ui.ctx().clone();
                self.load_chats(&ctx);
            }
        }
        if !self.log.is_empty() {
            egui::ScrollArea::vertical()
                .max_height(90.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for l in &self.log {
                        ui.small(l);
                    }
                });
        }
        if let Some(e) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, e);
        }
        if let Some(s) = &self.summary {
            ui.separator();
            ui.label(egui::RichText::new("Done").strong());
            egui::Grid::new("summary").show(ui, |ui| {
                for (label, n) in logic::summary_rows(s) {
                    ui.label(label);
                    ui.label(n.to_string());
                    ui.end_row();
                }
            });
            if let Some(dir) = &self.output_done
                && ui.button("Show in Finder").clicked()
            {
                let _ = Command::new("open").arg(dir).status();
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.drain();
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                Self::header(ui);
                ui.add_space(8.0);
                ui.add_enabled_ui(!self.running, |ui| {
                    ui.add_enabled_ui(self.session.is_none(), |ui| {
                        self.source_section(ui);
                        ui.add_space(8.0);
                        self.password_section(ui);
                    });
                    ui.add_space(8.0);
                    self.options_section(ui);
                });
                ui.add_space(8.0);
                self.run_section(ui);
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Stop any running job and give it a moment to delete its temporary backup.
        self.cancel.cancel();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.running && Instant::now() < deadline {
            if let Ok(Msg::Loaded(_) | Msg::Done(_)) =
                self.rx.recv_timeout(Duration::from_millis(100))
            {
                self.running = false;
            }
        }
        self.session = None;
    }
}

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([520.0, 680.0])
        .with_min_inner_size([420.0, 480.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/logo.png"))
    {
        viewport = viewport.with_icon(icon);
    }
    eframe::run_native(
        TITLE,
        eframe::NativeOptions {
            viewport,
            ..eframe::NativeOptions::default()
        },
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}
