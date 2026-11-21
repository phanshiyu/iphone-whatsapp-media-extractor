//! Simple desktop app: extract WhatsApp media from an iPhone into per-chat folders.
#![expect(missing_docs, reason = "internal modules of a binary crate")]

pub mod logic;
pub mod worker;

use eframe::egui;
use iwme_core::device::{self, BackupInfo};
use iwme_core::error::{self, is_cancelled};
use iwme_core::tools::{find_tool, home_dir};
use iwme_core::{CancelToken, Options, Progress, Summary};
use logic::{SourceMode, StartInputs};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use worker::{DeviceEntry, Job, Msg, Source};

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
                Msg::Done(r) => {
                    self.running = false;
                    match r {
                        Ok(s) => {
                            self.summary = Some(s);
                            self.output_done = Some(PathBuf::from(&self.output));
                        }
                        Err(e) if is_cancelled(&e) => {
                            self.error = Some("Cancelled.".to_owned());
                        }
                        Err(e) => {
                            self.fda_needed = logic::is_full_disk_access(&e);
                            self.error = Some(logic::friendly_error(&e));
                        }
                    }
                }
            }
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
            output_set: !self.output.trim().is_empty(),
            needs_password: self.needs_password(),
            password_set: !self.password.is_empty(),
            running: self.running,
        }
    }

    fn start(&mut self, ctx: &egui::Context) {
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
        let options = Options {
            backup_dir: PathBuf::new(),
            output_dir: PathBuf::from(self.output.trim()),
            password: self.needs_password().then(|| self.password.clone()),
            business: self.business,
            include_status: self.include_status,
            utc: self.utc,
            dry_run: false,
            exif: self.exif && self.exiftool_ok,
        };
        self.cancel = CancelToken::new();
        self.running = true;
        self.error = None;
        self.summary = None;
        self.fda_needed = false;
        self.log.clear();
        self.fraction = None;
        "Starting…".clone_into(&mut self.phase);
        worker::start(
            Job { source, options },
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
        if self.needs_password() {
            ui.label(egui::RichText::new("Backup password").strong());
            ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .password(true)
                    .desired_width(300.0),
            );
        }
        ui.collapsing("More options", |ui| {
            ui.checkbox(&mut self.business, "WhatsApp Business");
            ui.checkbox(&mut self.include_status, "Include status updates");
            ui.checkbox(&mut self.utc, "Use UTC in file names");
            ui.add_enabled(
                self.exiftool_ok,
                egui::Checkbox::new(&mut self.exif, "Write EXIF dates"),
            )
            .on_disabled_hover_text("Needs exiftool: brew install exiftool");
        });
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
        } else {
            let go = egui::Button::new(egui::RichText::new("Extract").size(18.0))
                .min_size(egui::vec2(ui.available_width(), 40.0));
            if ui
                .add_enabled(self.start_inputs().can_start(), go)
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start(&ctx);
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
                    self.source_section(ui);
                    ui.add_space(8.0);
                    self.options_section(ui);
                });
                ui.add_space(8.0);
                self.run_section(ui);
            });
        });
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
