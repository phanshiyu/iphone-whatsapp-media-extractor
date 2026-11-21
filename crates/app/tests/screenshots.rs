//! Renders the README screenshots headlessly (needs a GPU, so it is ignored by default).
//!
//! Regenerate with: `cargo test -p iwme-app --test screenshots -- --ignored`
//! Output: `docs/screenshots/{1-source,2-chats,3-progress,4-done}.png`.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable as _;
use iwme_app::App;
use iwme_app::logic;
use iwme_app::worker::DeviceEntry;
use iwme_core::chats::ChatInfo;
use iwme_core::device::Device;
use iwme_core::{Dedupe, Progress, Summary};
use std::path::{Path, PathBuf};

const WIDTH: f32 = 600.0;
const SCALE: f32 = 2.0;
const NOW: i64 = 1_795_176_000; // 2026-11-20 12:00 UTC
const DAY: i64 = 86_400;

fn chat(name: &str, is_group: bool, media_count: usize, days_ago: i64) -> ChatInfo {
    ChatInfo {
        key: format!(
            "{name}@{}",
            if is_group { "g.us" } else { "s.whatsapp.net" }
        ),
        name: name.to_owned(),
        is_group,
        media_count,
        first: NOW - 900 * DAY,
        last: NOW - days_ago * DAY,
    }
}

fn base() -> App {
    let mut app = App::blank();
    app.missing_tools.clear();
    app.exiftool_ok = true;
    "/Users/alex/Desktop/WhatsApp Media".clone_into(&mut app.output);
    app.devices = vec![DeviceEntry {
        device: Device {
            udid: "00008110-000A1B2C3D4E5F6A".to_owned(),
            name: Some("Alex's iPhone".to_owned()),
        },
        will_encrypt: Some(false),
    }];
    app
}

fn chats_scene() -> App {
    let mut app = base();
    let chats = vec![
        chat("Family", true, 1_842, 0),
        chat("Mum", false, 936, 1),
        chat("Book club", true, 214, 3),
        chat("Sam Lee", false, 587, 4),
        chat("Weekend hiking", true, 1_105, 9),
        chat("Priya Patel", false, 73, 21),
        chat("Flat 4B", true, 342, 48),
        chat("Dad", false, 129, 130),
    ];
    app.checked = chats
        .iter()
        .filter(|c| c.name != "Flat 4B" && c.name != "Dad")
        .map(|c| c.key.clone())
        .collect();
    app.chats = Some(chats);
    app.dedupe = Dedupe::PerChat;
    app
}

fn progress_scene() -> App {
    let mut app = chats_scene();
    app.running = true;
    let p = Progress::Extracting {
        done: 1_204,
        total: 4_980,
    };
    app.phase = logic::phase_text(&p);
    app.fraction = logic::fraction(&p);
    app.log = vec![
        "warning: IMG_2041.HEIC is not in the backup, skipping".to_owned(),
        "warning: VID-20230611-WA0007.mp4 is not in the backup, skipping".to_owned(),
    ];
    app
}

fn done_scene() -> App {
    let mut app = base();
    app.summary = Some(Summary {
        chats: 6,
        extracted: 4_731,
        up_to_date: 0,
        missing: 18,
        skipped: 0,
        failed: 0,
        duplicates: 231,
    });
    app.output_done = Some(PathBuf::from(&app.output));
    app
}

/// Crop the harness margin and the empty space below the content.
fn trim(img: &image::RgbaImage) -> image::RgbaImage {
    let (w, h) = img.dimensions();
    let edge = 16;
    let bg = *img.get_pixel(w / 2, h - edge - 1);
    let last = (edge..h - edge)
        .rev()
        .find(|&y| (edge..w - edge).any(|x| *img.get_pixel(x, y) != bg))
        .unwrap_or(h - edge);
    let bottom = (last + 24).min(h - edge);
    image::imageops::crop_imm(img, edge, edge, w - 2 * edge, bottom - edge).to_image()
}

fn render(app: App, name: &str, open_options: bool) -> anyhow::Result<()> {
    let ctx_setup = |ctx: &egui::Context| {
        egui_extras::install_image_loaders(ctx);
        ctx.set_theme(egui::Theme::Light);
    };
    let builder = Harness::builder()
        .wgpu()
        .with_size(egui::vec2(WIDTH, 1000.0))
        .with_pixels_per_point(SCALE)
        .with_accessibility_check(false)
        .with_render_cursor(false)
        .with_theme(egui::Theme::Light)
        .with_context_setup(ctx_setup);
    let mut harness = builder.build_ui_state(|ui, app: &mut App| app.show(ui), app);
    harness.run_ok();
    if open_options {
        harness.get_by_label("More options").click();
        harness.run_ok();
    }
    harness.run_ok();
    let img = trim(&harness.render().map_err(|e| anyhow::anyhow!(e))?);
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/screenshots");
    img.save(dir.join(name))?;
    Ok(())
}

#[test]
#[ignore = "needs a GPU; run manually to regenerate the README screenshots"]
fn screenshots() -> anyhow::Result<()> {
    render(base(), "1-source.png", false)?;
    render(chats_scene(), "2-chats.png", true)?;
    render(progress_scene(), "3-progress.png", false)?;
    render(done_scene(), "4-done.png", false)?;
    Ok(())
}
