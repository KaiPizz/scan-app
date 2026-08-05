#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod atomic_file;
mod autocapture;
mod camera;
mod document;
mod library;
mod library_view;
mod overlay;
mod pipeline;
mod review_viewport;
mod session;
mod storage;

use app::DocumentScannerApp;
use eframe::egui;

fn main() -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Skaner dokumentów")
            .with_inner_size([1000.0, 440.0])
            .with_min_inner_size([720.0, 420.0])
            .with_maximized(true),
        ..Default::default()
    };

    eframe::run_native(
        "Skaner dokumentów",
        native_options,
        Box::new(|creation_context| Ok(Box::new(DocumentScannerApp::new(creation_context)))),
    )
}
