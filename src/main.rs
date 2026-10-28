#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod hid;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([480.0, 700.0])
            .with_min_inner_size([440.0, 660.0])
            .with_title("VT1 Pro 控制面板"),
        renderer: eframe::Renderer::Glow,
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "VT1 Pro",
        options,
        Box::new(|cc| Ok(Box::new(app::Panel::new(cc)))),
    )
}
