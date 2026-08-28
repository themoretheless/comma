mod app;
mod config;
mod input;
mod pty;
mod render;
mod tab;
mod tabs;

/// The bundled egui fonts lack technical glyphs (⌘, ⎇, ❯); append macOS
/// system fonts as fallbacks so UI chrome renders them instead of tofu.
fn install_font_fallbacks(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, path) in [
        ("apple-symbols", "/System/Library/Fonts/Apple Symbols.ttf"),
        ("menlo", "/System/Library/Fonts/Menlo.ttc"),
    ] {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        fonts.font_data.insert(name.into(), egui::FontData::from_owned(bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().push(name.into());
        }
    }
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result<()> {
    let config = config::Config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("comma")
            .with_inner_size([config.window_width, config.window_height])
            // Content extends to the very top edge, under the hidden title
            // bar (traffic lights stay, the area still drags the window).
            .with_titlebar_shown(false)
            .with_fullsize_content_view(true),
        ..Default::default()
    };
    eframe::run_native(
        "comma",
        options,
        Box::new(move |cc| {
            install_font_fallbacks(&cc.egui_ctx);
            Ok(Box::new(app::CommaApp::new(cc, config)))
        }),
    )
}
