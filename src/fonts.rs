use std::sync::Arc;

/// Registers Noto Sans JP (SIL OFL 1.1, bundled under assets/fonts/) as a
/// fallback font so Japanese glyphs render correctly. egui's default fonts
/// only cover Latin scripts, so without this Japanese text shows as tofu
/// boxes.
pub fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    fonts.font_data.insert(
        "noto_sans_jp".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/fonts/NotoSansJP-Regular.otf"
        ))),
    );

    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .push("noto_sans_jp".to_owned());
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .push("noto_sans_jp".to_owned());

    ctx.set_fonts(fonts);
}
