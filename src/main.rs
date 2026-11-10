// Only suppress the console in release builds - keeping it in debug builds
// means println!/eprintln! and panic messages are still visible when running
// via `cargo run`, which is worth more than a hidden console during dev.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod archive;
mod cli;
mod fonts;
mod media;
mod thumbnails;
mod video;
mod workspace;

use app::KotoViewApp;

/// Decodes the same source image kotoview's Tauri build generated its icon
/// set from, for the window/taskbar icon. The .exe's own icon (as seen in
/// Explorer) is a separate embedding, done in build.rs via winres.
fn load_icon() -> egui::IconData {
    let bytes = include_bytes!("../icon-source.png");
    let image = image::load_from_memory(bytes)
        .expect("bundled icon-source.png should always decode")
        .to_rgba8();
    let (width, height) = image.dimensions();
    egui::IconData {
        rgba: image.into_raw(),
        width,
        height,
    }
}

fn main() -> eframe::Result<()> {
    let initial_target = cli::resolve_initial_target();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1200.0, 800.0])
            .with_icon(load_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "KotoView",
        native_options,
        Box::new(|cc| {
            fonts::setup(&cc.egui_ctx);
            Ok(Box::new(KotoViewApp::new(initial_target)))
        }),
    )
}
