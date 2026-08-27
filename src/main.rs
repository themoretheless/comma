mod app;
mod config;
mod input;
mod pty;
mod render;
mod tab;
mod tabs;

fn main() -> eframe::Result<()> {
    let config = config::Config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("comma")
            .with_inner_size([config.window_width, config.window_height]),
        ..Default::default()
    };
    eframe::run_native(
        "comma",
        options,
        Box::new(move |cc| Ok(Box::new(app::CommaApp::new(cc, config)))),
    )
}
