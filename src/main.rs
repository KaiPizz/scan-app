#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod camera;
mod document;
mod storage;

use app::DocumentScannerApp;
use eframe::egui;

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Skaner dokumentów")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 640.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Skaner dokumentów",
        native_options,
        Box::new(|creation_context| Ok(Box::new(DocumentScannerApp::new(creation_context)))),
    )
}
