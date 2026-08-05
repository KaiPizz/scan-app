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
    let instance_lock = match storage::acquire_instance_lock() {
        Ok(lock) => lock,
        Err(message) => return run_already_running_notice(message),
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Skaner dokumentów")
            .with_inner_size([1000.0, 440.0])
            .with_min_inner_size([720.0, 420.0])
            .with_maximized(true),
        ..Default::default()
    };

    let result = eframe::run_native(
        "Skaner dokumentów",
        native_options,
        Box::new(|creation_context| Ok(Box::new(DocumentScannerApp::new(creation_context)))),
    );
    drop(instance_lock);
    result
}

struct AlreadyRunningNotice {
    message: String,
}

impl eframe::App for AlreadyRunningNotice {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.vertical_centered(|ui| {
            ui.add_space(36.0);
            ui.heading("Skaner dokumentów jest już uruchomiony");
            ui.add_space(8.0);
            ui.label(&self.message);
            ui.label("Przełącz się na otwarte okno programu.");
            ui.add_space(16.0);
            if ui.button("Zamknij").clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

fn run_already_running_notice(message: String) -> eframe::Result {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Skaner dokumentów")
            .with_inner_size([460.0, 220.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "Skaner dokumentów — już uruchomiony",
        native_options,
        Box::new(|_| Ok(Box::new(AlreadyRunningNotice { message }))),
    )
}
