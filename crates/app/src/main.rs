//! Desktop entry point.

use iwme_app::{App, TITLE};

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
