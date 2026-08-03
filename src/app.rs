use crate::autocapture::{AutoCapture, FeedResult};
use crate::camera::{CameraController, CameraEvent};
use crate::document::{CropPoint, ScannedPage, rotate_page_clockwise, save_pdf};
use crate::overlay::OverlayDetector;
use crate::pipeline::{PipelineEvent, ProcessingPipeline};
use crate::storage::{
    FolderInfo, PdfInfo, Settings, create_folder, default_library_root, ensure_library,
    list_folders, list_pdfs, load_settings, rename_folder, save_settings, unique_pdf_path,
};
use eframe::egui::{
    self, Align, Button, Color32, ColorImage, CornerRadius, FontId, Frame, Id, Layout, Margin,
    Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, UiBuilder, Vec2,
};
use image::RgbImage;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const BLUE: Color32 = Color32::from_rgb(38, 101, 180);
const BLUE_DARK: Color32 = Color32::from_rgb(24, 72, 130);
const PALE_BLUE: Color32 = Color32::from_rgb(231, 241, 252);
const BACKGROUND: Color32 = Color32::from_rgb(246, 248, 251);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Library,
    Folder,
    ScanHub,
}

struct PageData {
    page: ScannedPage,
    original_jpeg: Vec<u8>,
    corners: [CropPoint; 4],
    texture: TextureHandle,
}

enum PageSlot {
    Processing,
    Ready(Box<PageData>),
    Failed {
        original_jpeg: Vec<u8>,
        error: String,
    },
    Reprocessing {
        original_jpeg: Vec<u8>,
    },
}

struct EditorState {
    slot_index: usize,
    original: RgbImage,
    texture: TextureHandle,
    corners: [CropPoint; 4],
}

struct SlotEntry {
    id: u64,
    slot: PageSlot,
}

struct Toast {
    text: String,
    shown_at: Instant,
}

pub struct DocumentScannerApp {
    screen: Screen,
    settings: Settings,
    library_root: PathBuf,
    folders: Vec<FolderInfo>,
    selected_folder: Option<FolderInfo>,
    pdfs: Vec<PdfInfo>,

    camera: Option<CameraController>,
    camera_status: String,
    camera_ready: bool,
    preview_texture: Option<TextureHandle>,
    preview_size: [usize; 2],

    slots: Vec<SlotEntry>,
    selected_slot: Option<usize>,
    next_page_id: u64,
    pending_jobs: usize,
    pipeline: Option<ProcessingPipeline>,
    overlay: Option<OverlayDetector>,
    autocapture: AutoCapture,
    pending_preview: Option<RgbImage>,
    editor: Option<EditorState>,
    filename: String,
    toast: Option<Toast>,

    show_new_folder: bool,
    new_folder_name: String,
    show_rename_folder: bool,
    rename_folder_name: String,
    show_settings: bool,
    show_save: bool,
    save_dialog_needs_focus: bool,
    show_cancel_confirm: bool,
    show_delete_confirm: bool,
    show_exit_confirm: bool,
    allow_exit: bool,
    message: Option<String>,
}

impl DocumentScannerApp {
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        configure_style(&context.egui_ctx);
        let settings = load_settings();
        let library_root = settings
            .library_root
            .clone()
            .unwrap_or_else(default_library_root);
        let mut app = Self {
            screen: Screen::Library,
            settings,
            library_root,
            folders: Vec::new(),
            selected_folder: None,
            pdfs: Vec::new(),
            camera: None,
            camera_status: String::new(),
            camera_ready: false,
            preview_texture: None,
            preview_size: [0, 0],
            slots: Vec::new(),
            selected_slot: None,
            next_page_id: 0,
            pending_jobs: 0,
            pipeline: None,
            overlay: None,
            autocapture: AutoCapture::new(),
            pending_preview: None,
            editor: None,
            filename: String::new(),
            toast: None,
            show_new_folder: false,
            new_folder_name: String::new(),
            show_rename_folder: false,
            rename_folder_name: String::new(),
            show_settings: false,
            show_save: false,
            save_dialog_needs_focus: false,
            show_cancel_confirm: false,
            show_delete_confirm: false,
            show_exit_confirm: false,
            allow_exit: false,
            message: None,
        };
        if let Err(error) = ensure_library(&app.library_root) {
            app.message = Some(error);
        }
        app.refresh_folders();
        app.restore_last_folder();
        app
    }

    fn restore_last_folder(&mut self) {
        let Some(last_name) = self.settings.last_folder.clone() else {
            return;
        };
        let Some(folder) = self
            .folders
            .iter()
            .find(|folder| folder.name == last_name)
            .cloned()
        else {
            return;
        };
        self.open_folder(folder);
    }

    fn refresh_folders(&mut self) {
        match list_folders(&self.library_root) {
            Ok(folders) => self.folders = folders,
            Err(error) => self.message = Some(error),
        }
    }

    fn refresh_pdfs(&mut self) {
        let Some(folder) = &self.selected_folder else {
            self.pdfs.clear();
            return;
        };
        match list_pdfs(&folder.path) {
            Ok(pdfs) => self.pdfs = pdfs,
            Err(error) => self.message = Some(error),
        }
    }

    fn open_folder(&mut self, folder: FolderInfo) {
        self.settings.last_folder = Some(folder.name.clone());
        self.selected_folder = Some(folder);
        self.screen = Screen::Folder;
        self.refresh_pdfs();
        let _ = save_settings(&self.settings);
    }

    fn back_to_library(&mut self) {
        self.selected_folder = None;
        self.settings.last_folder = None;
        self.screen = Screen::Library;
        self.refresh_folders();
        let _ = save_settings(&self.settings);
    }

    fn begin_scan(&mut self) {
        self.slots.clear();
        self.selected_slot = None;
        self.pending_jobs = 0;
        self.filename.clear();
        self.pipeline = Some(ProcessingPipeline::start());
        self.overlay = Some(OverlayDetector::start());
        self.autocapture = AutoCapture::new();
        self.start_camera();
    }

    fn start_camera(&mut self) {
        self.stop_camera();
        self.camera_status = "Łączenie z IRIScan Visualizer 7…".to_owned();
        self.camera_ready = false;
        self.preview_texture = None;
        self.camera = Some(CameraController::start());
        self.screen = Screen::ScanHub;
    }

    fn stop_camera(&mut self) {
        if let Some(mut camera) = self.camera.take() {
            camera.stop();
        }
    }

    fn poll_camera(&mut self, context: &egui::Context) {
        let mut events = Vec::new();
        if let Some(camera) = &self.camera {
            while let Some(event) = camera.try_event() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                CameraEvent::Ready {
                    device_name,
                    width,
                    height,
                } => {
                    self.camera_ready = true;
                    self.camera_status = format!("{device_name} · {width} × {height} px");
                }
                CameraEvent::Preview(image) => {
                    self.update_preview_texture(context, &image);
                    if let Some(overlay) = &self.overlay {
                        overlay.submit(image.clone());
                    }
                    self.pending_preview = Some(image);
                }
                CameraEvent::Error(error) => {
                    self.camera_ready = false;
                    self.preview_texture = None;
                    self.camera_status = error;
                }
            }
        }
        if self.screen == Screen::ScanHub {
            context.request_repaint_after(Duration::from_millis(35));
        }
    }

    fn update_preview_texture(&mut self, context: &egui::Context, image: &RgbImage) {
        let color_image = rgb_to_color_image(image);
        self.preview_size = color_image.size;
        if let Some(texture) = &mut self.preview_texture {
            texture.set(color_image, TextureOptions::LINEAR);
        } else {
            self.preview_texture =
                Some(context.load_texture("podglad-kamery", color_image, TextureOptions::LINEAR));
        }
    }

    fn capture(&mut self, manual: bool) {
        let Some(frame) = self
            .camera
            .as_ref()
            .and_then(CameraController::latest_full_image)
        else {
            if manual {
                self.message = Some("Poczekaj, aż pojawi się obraz z kamery.".to_owned());
            }
            return;
        };
        let Some(pipeline) = &self.pipeline else {
            return;
        };
        let id = self.next_page_id;
        if pipeline.try_submit(id, frame) {
            self.next_page_id += 1;
            self.slots.push(SlotEntry {
                id,
                slot: PageSlot::Processing,
            });
            self.pending_jobs += 1;
            if manual {
                self.autocapture.note_manual_capture();
            }
            beep();
        } else if manual {
            self.message = Some("Poczekaj — przetwarzanie poprzednich stron…".to_owned());
        }
    }

    fn poll_pipeline(&mut self, context: &egui::Context) {
        let mut events = Vec::new();
        if let Some(pipeline) = &self.pipeline {
            while let Some(event) = pipeline.try_event() {
                events.push(event);
            }
        }
        for event in events {
            self.pending_jobs = self.pending_jobs.saturating_sub(1);
            match event {
                PipelineEvent::PageReady {
                    id,
                    page,
                    original_jpeg,
                    corners,
                } => {
                    let texture = context.load_texture(
                        format!("strona-{id}"),
                        rgb_to_color_image(&page.review_image),
                        TextureOptions::LINEAR,
                    );
                    if let Some(entry) = self.slots.iter_mut().find(|entry| entry.id == id) {
                        entry.slot = PageSlot::Ready(Box::new(PageData {
                            page,
                            original_jpeg,
                            corners,
                            texture,
                        }));
                    }
                }
                PipelineEvent::PageFailed {
                    id,
                    original_jpeg,
                    error,
                } => {
                    if let Some(entry) = self.slots.iter_mut().find(|entry| entry.id == id) {
                        entry.slot = PageSlot::Failed {
                            original_jpeg,
                            error,
                        };
                    }
                }
                PipelineEvent::ReprocessDone { id, page, corners } => {
                    let texture = context.load_texture(
                        format!("strona-{id}-kadr"),
                        rgb_to_color_image(&page.review_image),
                        TextureOptions::LINEAR,
                    );
                    if let Some(entry) = self.slots.iter_mut().find(|entry| entry.id == id) {
                        let original_jpeg = match &mut entry.slot {
                            PageSlot::Reprocessing { original_jpeg } => {
                                std::mem::take(original_jpeg)
                            }
                            _ => Vec::new(),
                        };
                        entry.slot = PageSlot::Ready(Box::new(PageData {
                            page,
                            original_jpeg,
                            corners,
                            texture,
                        }));
                    }
                }
                PipelineEvent::ReprocessFailed { id, error } => {
                    if let Some(entry) = self.slots.iter_mut().find(|entry| entry.id == id) {
                        let original_jpeg = match &mut entry.slot {
                            PageSlot::Reprocessing { original_jpeg } => {
                                std::mem::take(original_jpeg)
                            }
                            _ => Vec::new(),
                        };
                        entry.slot = PageSlot::Failed {
                            original_jpeg,
                            error,
                        };
                    }
                }
            }
        }
        if self.pending_jobs > 0 {
            context.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn can_save(&self) -> bool {
        !self.slots.is_empty()
            && self.pending_jobs == 0
            && self
                .slots
                .iter()
                .all(|entry| matches!(entry.slot, PageSlot::Ready(_)))
    }

    fn rotate_selected_page(&mut self, context: &egui::Context) {
        let Some(index) = self.selected_slot else {
            return;
        };
        let Some(entry) = self.slots.get_mut(index) else {
            return;
        };
        let PageSlot::Ready(data) = &mut entry.slot else {
            return;
        };
        match rotate_page_clockwise(&data.page) {
            Ok(rotated) => {
                data.texture = context.load_texture(
                    format!("strona-{}-obrot-{}", entry.id, rotated.width),
                    rgb_to_color_image(&rotated.review_image),
                    TextureOptions::LINEAR,
                );
                data.page = rotated;
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn delete_selected_page(&mut self) {
        let Some(index) = self.selected_slot else {
            return;
        };
        if index >= self.slots.len() {
            self.selected_slot = None;
            return;
        }
        self.slots.remove(index);
        self.selected_slot = if self.slots.is_empty() {
            None
        } else {
            Some(index.min(self.slots.len() - 1))
        };
    }

    fn move_selected_page(&mut self, direction: isize) {
        let Some(index) = self.selected_slot else {
            return;
        };
        let target = index as isize + direction;
        if !(0..self.slots.len() as isize).contains(&target) {
            return;
        }
        let target = target as usize;
        self.slots.swap(index, target);
        self.selected_slot = Some(target);
    }

    fn save_current_document(&mut self) {
        let Some(folder) = &self.selected_folder else {
            self.message = Some("Najpierw wybierz folder docelowy.".to_owned());
            return;
        };
        let folder_path = folder.path.clone();
        let mut pages = Vec::with_capacity(self.slots.len());
        for entry in &self.slots {
            match &entry.slot {
                PageSlot::Ready(data) => pages.push(&data.page),
                _ => {
                    self.message = Some("Usuń strony z błędem (⚠) przed zapisem.".to_owned());
                    return;
                }
            }
        }
        let path = match unique_pdf_path(&folder_path, &self.filename) {
            Ok(path) => path,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        match save_pdf(&path, &pages) {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.toast = Some(Toast {
                    text: format!("Zapisano: {name}"),
                    shown_at: Instant::now(),
                });
                self.show_save = false;
                self.slots.clear();
                self.selected_slot = None;
                self.filename.clear();
                self.refresh_pdfs();
                self.refresh_folders();
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn open_editor(&mut self, index: usize, context: &egui::Context) {
        let Some(entry) = self.slots.get(index) else {
            return;
        };
        let (original_jpeg, corners) = match &entry.slot {
            PageSlot::Ready(data) => (&data.original_jpeg, data.corners),
            PageSlot::Failed { original_jpeg, .. } => (original_jpeg, fallback_editor_corners()),
            _ => return,
        };
        if original_jpeg.is_empty() {
            self.message =
                Some("Ta strona nie ma zapisanego oryginału (odzyskana sesja).".to_owned());
            return;
        }
        match image::load_from_memory(original_jpeg) {
            Ok(image) => {
                let original = image.to_rgb8();
                let texture = context.load_texture(
                    format!("edytor-{}", entry.id),
                    rgb_to_color_image(&original),
                    TextureOptions::LINEAR,
                );
                self.editor = Some(EditorState {
                    slot_index: index,
                    original,
                    texture,
                    corners,
                });
            }
            Err(error) => self.message = Some(format!("Nie można odczytać oryginału: {error}")),
        }
    }

    fn apply_editor(&mut self) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let Some(entry) = self.slots.get_mut(editor.slot_index) else {
            return;
        };
        let Some(pipeline) = &self.pipeline else {
            return;
        };
        let original_jpeg = match &mut entry.slot {
            PageSlot::Ready(data) => std::mem::take(&mut data.original_jpeg),
            PageSlot::Failed { original_jpeg, .. } => std::mem::take(original_jpeg),
            _ => return,
        };
        if pipeline.submit_reprocess(entry.id, Arc::new(editor.original), editor.corners) {
            entry.slot = PageSlot::Reprocessing { original_jpeg };
            self.pending_jobs += 1;
        } else {
            entry.slot = PageSlot::Failed {
                original_jpeg,
                error: "Kolejka przetwarzania jest pełna. Spróbuj ponownie.".to_owned(),
            };
        }
    }

    fn request_cancel_scan(&mut self) {
        if self.slots.is_empty() {
            self.abandon_scan();
        } else {
            self.show_cancel_confirm = true;
        }
    }

    fn abandon_scan(&mut self) {
        self.stop_camera();
        self.pipeline = None;
        self.overlay = None;
        self.slots.clear();
        self.selected_slot = None;
        self.pending_jobs = 0;
        self.screen = Screen::Folder;
        self.refresh_pdfs();
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        Frame::new()
            .fill(Color32::WHITE)
            .inner_margin(Margin::symmetric(24, 14))
            .show(ui, |ui| {
                two_sided(
                    ui,
                    42.0,
                    |ui| {
                        ui.label(
                            RichText::new("Skaner dokumentów")
                                .size(25.0)
                                .strong()
                                .color(BLUE_DARK),
                        );
                    },
                    |ui| {
                        if ui.button("Ustawienia").clicked() {
                            self.show_settings = true;
                        }
                        if let Some(folder) = &self.selected_folder {
                            ui.label(
                                RichText::new(&folder.name)
                                    .size(16.0)
                                    .color(Color32::DARK_GRAY),
                            );
                        }
                    },
                );
            });
    }

    fn library_ui(&mut self, ui: &mut egui::Ui) {
        page_container(ui, |ui| {
            two_sided(
                ui,
                58.0,
                |ui| {
                    ui.vertical(|ui| {
                        ui.heading("Foldery dokumentów");
                        ui.label("Wybierz folder lub utwórz nowy.");
                    });
                },
                |ui| {
                    if primary_button(ui, "Nowy folder").clicked() {
                        self.new_folder_name.clear();
                        self.show_new_folder = true;
                    }
                },
            );
            ui.add_space(22.0);
            if self.folders.is_empty() {
                empty_card(
                    ui,
                    "Nie ma jeszcze żadnych folderów.",
                    "Kliknij „Nowy folder”, aby rozpocząć.",
                );
                return;
            }
            let tile_width = 235.0;
            let columns = ((ui.available_width() / (tile_width + 14.0)).floor() as usize).max(1);
            let mut clicked_folder = None;
            egui::Grid::new("foldery")
                .num_columns(columns)
                .spacing([14.0, 14.0])
                .show(ui, |ui| {
                    for (index, folder) in self.folders.iter().enumerate() {
                        let label = format!("{}\n\n{} plików PDF", folder.name, folder.pdf_count);
                        if ui
                            .add_sized(
                                [tile_width, 125.0],
                                Button::new(RichText::new(label).size(17.0))
                                    .fill(Color32::WHITE)
                                    .stroke(Stroke::new(1.0, Color32::from_gray(215)))
                                    .corner_radius(12.0),
                            )
                            .clicked()
                        {
                            clicked_folder = Some(folder.clone());
                        }
                        if (index + 1) % columns == 0 {
                            ui.end_row();
                        }
                    }
                });
            if let Some(folder) = clicked_folder {
                self.open_folder(folder);
            }
        });
    }

    fn folder_ui(&mut self, ui: &mut egui::Ui) {
        page_container(ui, |ui| {
            let folder_name = self
                .selected_folder
                .as_ref()
                .map(|folder| folder.name.clone())
                .unwrap_or_default();
            let folder_path = self
                .selected_folder
                .as_ref()
                .map(|folder| folder.path.clone());
            let (go_back, (new_scan, rename, open_in_explorer)) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut go_back = false;
                    ui.horizontal(|ui| {
                        go_back = ui.button("Wróć do folderów").clicked();
                        ui.add_space(8.0);
                        ui.heading(&folder_name);
                    });
                    go_back
                },
                |ui| {
                    let new_scan = primary_button(ui, "Nowy skan").clicked();
                    let rename = ui.button("Zmień nazwę folderu").clicked();
                    let open_in_explorer = ui.button("Otwórz w Eksploratorze").clicked();
                    (new_scan, rename, open_in_explorer)
                },
            );
            if go_back {
                self.back_to_library();
                return;
            }
            if new_scan {
                self.begin_scan();
                return;
            }
            if rename {
                self.rename_folder_name = folder_name;
                self.show_rename_folder = true;
            }
            if open_in_explorer
                && let Some(path) = folder_path
                && let Err(error) = open::that_detached(path)
            {
                self.message = Some(format!("Nie można otworzyć folderu: {error}"));
            }
            ui.add_space(20.0);
            if self.pdfs.is_empty() {
                empty_card(
                    ui,
                    "Ten folder jest pusty.",
                    "Kliknij „Nowy skan”, aby dodać pierwszy dokument.",
                );
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for pdf in &self.pdfs {
                        Frame::new()
                            .fill(Color32::WHITE)
                            .corner_radius(10.0)
                            .stroke(Stroke::new(1.0, Color32::from_gray(222)))
                            .inner_margin(Margin::symmetric(18, 14))
                            .show(ui, |ui| {
                                let (_, open_pdf) = two_sided(
                                    ui,
                                    38.0,
                                    |ui| {
                                        ui.label(RichText::new(&pdf.name).size(16.0));
                                    },
                                    |ui| ui.button("Otwórz PDF").clicked(),
                                );
                                if open_pdf && let Err(error) = open::that_detached(&pdf.path) {
                                    self.message = Some(format!("Nie można otworzyć PDF: {error}"));
                                }
                            });
                        ui.add_space(8.0);
                    }
                });
            }
        });
    }

    fn scan_hub_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        self.poll_camera(context);
        self.poll_pipeline(context);
        let dialog_open = self.show_save
            || self.show_cancel_confirm
            || self.show_delete_confirm
            || self.show_settings
            || self.show_new_folder
            || self.show_rename_folder
            || self.show_exit_confirm
            || self.message.is_some();
        if let Some(preview) = self.pending_preview.take()
            && self.camera_ready
            && !dialog_open
            && self.editor.is_none()
            && self.autocapture.feed(&preview, Instant::now()) == FeedResult::Trigger
        {
            self.capture(false);
        }
        if self.editor.is_some() {
            self.editor_ui(ui, context);
            return;
        }
        page_container(ui, |ui| {
            let camera_ready = self.camera_ready;
            let camera_status = self.camera_status.clone();
            let page_count_text = polish_page_count(self.slots.len());
            let slots_empty = self.slots.is_empty();
            let ((cancel, back), ()) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut cancel = false;
                    let mut back = false;
                    ui.horizontal(|ui| {
                        cancel = ui.button("Anuluj dokument").clicked();
                        back = ui
                            .add_enabled(slots_empty, Button::new("Wróć do folderu"))
                            .clicked();
                        ui.add_space(10.0);
                        ui.heading(format!("Skanowanie · {page_count_text}"));
                    });
                    (cancel, back)
                },
                |ui| {
                    let color = if camera_ready {
                        Color32::DARK_GREEN
                    } else {
                        Color32::DARK_GRAY
                    };
                    ui.label(RichText::new(camera_status).color(color));
                    let auto_on = self.autocapture.enabled();
                    let toggle_label = if auto_on { "Auto: WŁ" } else { "Auto: WYŁ" };
                    let toggle = Button::new(
                        RichText::new(toggle_label)
                            .strong()
                            .color(if auto_on { Color32::WHITE } else { BLUE_DARK }),
                    )
                    .fill(if auto_on { BLUE } else { Color32::WHITE })
                    .stroke(Stroke::new(1.0, BLUE))
                    .corner_radius(9.0);
                    if ui.add(toggle).clicked() {
                        self.autocapture.set_enabled(!auto_on);
                    }
                    if camera_ready {
                        ui.label(
                            RichText::new(self.autocapture.hint()).color(Color32::from_gray(90)),
                        );
                    }
                },
            );
            if cancel {
                self.request_cancel_scan();
                return;
            }
            if back {
                self.abandon_scan();
                return;
            }
            ui.add_space(10.0);

            let controls_min = ui.available_rect_before_wrap().min;
            let controls_width = (ui.clip_rect().right() - controls_min.x).max(0.0);
            let (controls_rect, _) =
                ui.allocate_exact_size(Vec2::new(controls_width, 54.0), Sense::hover());
            let mut controls_ui = ui.new_child(
                UiBuilder::new()
                    .id_salt("scanhub-controls")
                    .max_rect(controls_rect)
                    .layout(Layout::left_to_right(Align::Center).with_main_align(Align::Center)),
            );
            let capture_clicked = controls_ui
                .add_enabled(
                    self.camera_ready && self.preview_texture.is_some(),
                    Button::new(
                        RichText::new("Zrób zdjęcie (Spacja)")
                            .size(20.0)
                            .strong()
                            .color(Color32::WHITE),
                    )
                    .fill(BLUE)
                    .corner_radius(12.0)
                    .min_size(Vec2::new(260.0, 54.0)),
                )
                .clicked();
            if capture_clicked {
                self.capture(true);
            }
            let save_label = if self.pending_jobs > 0 {
                format!("Przetwarzanie {} stron…", self.pending_jobs)
            } else {
                "Zapisz dokument (Enter)".to_owned()
            };
            if controls_ui
                .add_enabled(
                    self.can_save(),
                    Button::new(RichText::new(save_label).strong().color(Color32::WHITE))
                        .fill(BLUE_DARK)
                        .corner_radius(12.0)
                        .min_size(Vec2::new(240.0, 54.0)),
                )
                .clicked()
            {
                self.save_dialog_needs_focus = true;
                self.show_save = true;
            }
            if !self.camera_ready && controls_ui.button("Spróbuj ponownie").clicked() {
                self.start_camera();
            }
            ui.add_space(8.0);

            if let Some(index) = self.selected_slot {
                if index >= self.slots.len() {
                    self.selected_slot = None;
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("Strona {} z {}", index + 1, self.slots.len()))
                                .strong(),
                        );
                        let can_left = index > 0;
                        let can_right = index + 1 < self.slots.len();
                        if ui.add_enabled(can_left, Button::new("← W lewo")).clicked() {
                            self.move_selected_page(-1);
                        }
                        if ui
                            .add_enabled(can_right, Button::new("W prawo →"))
                            .clicked()
                        {
                            self.move_selected_page(1);
                        }
                        let rotatable = matches!(
                            self.slots.get(index).map(|entry| &entry.slot),
                            Some(PageSlot::Ready(_))
                        );
                        if ui.add_enabled(rotatable, Button::new("Obróć")).clicked() {
                            self.rotate_selected_page(context);
                        }
                        let editable = matches!(
                            self.slots.get(index).map(|entry| &entry.slot),
                            Some(PageSlot::Ready(_)) | Some(PageSlot::Failed { .. })
                        );
                        if ui
                            .add_enabled(editable, Button::new("Popraw kadr"))
                            .clicked()
                        {
                            self.open_editor(index, context);
                        }
                        if ui
                            .button(RichText::new("Usuń stronę").color(Color32::DARK_RED))
                            .clicked()
                        {
                            self.show_delete_confirm = true;
                        }
                        if ui.button("Odznacz").clicked() {
                            self.selected_slot = None;
                        }
                    });
                    ui.add_space(6.0);
                }
            }

            const STRIP_HEIGHT: f32 = 160.0;
            let preview_min = ui.available_rect_before_wrap().min;
            let preview_max = Pos2::new(
                ui.clip_rect().right(),
                (ui.clip_rect().bottom() - STRIP_HEIGHT - 10.0).max(preview_min.y + 80.0),
            );
            if preview_max.x > preview_min.x && preview_max.y > preview_min.y {
                let preview_rect = Rect::from_min_max(preview_min, preview_max);
                ui.allocate_rect(preview_rect, Sense::hover());
                let painter = ui.painter_at(preview_rect);
                painter.rect_filled(preview_rect, 12.0, Color32::from_gray(30));
                if let Some(texture) = &self.preview_texture {
                    let image_bounds = preview_rect.shrink(12.0);
                    let size = fit_size(
                        Vec2::new(self.preview_size[0] as f32, self.preview_size[1] as f32),
                        image_bounds.size(),
                    );
                    let draw_rect = Rect::from_center_size(image_bounds.center(), size);
                    painter.image(
                        texture.id(),
                        draw_rect,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    if let Some(corners) = self.overlay.as_ref().and_then(OverlayDetector::latest)
                    {
                        let points = corners.map(|corner| {
                            Pos2::new(
                                draw_rect.left() + corner.x * draw_rect.width(),
                                draw_rect.top() + corner.y * draw_rect.height(),
                            )
                        });
                        for edge in 0..4 {
                            painter.line_segment(
                                [points[edge], points[(edge + 1) % 4]],
                                Stroke::new(3.0, Color32::from_rgb(70, 165, 255)),
                            );
                        }
                    }
                } else {
                    painter.text(
                        preview_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &self.camera_status,
                        FontId::proportional(18.0),
                        Color32::WHITE,
                    );
                }
            }

            let strip_min = Pos2::new(preview_min.x, preview_max.y + 10.0);
            let strip_max = Pos2::new(ui.clip_rect().right(), ui.clip_rect().bottom());
            if strip_max.x > strip_min.x && strip_max.y > strip_min.y {
                let strip_rect = Rect::from_min_max(strip_min, strip_max);
                ui.allocate_rect(strip_rect, Sense::hover());
                let mut strip_ui = ui.new_child(
                    UiBuilder::new()
                        .id_salt("film-strip")
                        .max_rect(strip_rect)
                        .layout(Layout::left_to_right(Align::Center)),
                );
                self.film_strip_ui(&mut strip_ui);
            }
        });
    }

    fn film_strip_ui(&mut self, ui: &mut egui::Ui) {
        let mut clicked = None;
        egui::ScrollArea::horizontal()
            .id_salt("film-strip-scroll")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (index, entry) in self.slots.iter().enumerate() {
                        let selected = self.selected_slot == Some(index);
                        let stroke = if selected {
                            Stroke::new(3.0, BLUE)
                        } else {
                            Stroke::new(1.0, Color32::from_gray(210))
                        };
                        let frame_response = Frame::new()
                            .fill(Color32::WHITE)
                            .stroke(stroke)
                            .corner_radius(8.0)
                            .inner_margin(6)
                            .show(ui, |ui| {
                                ui.set_width(104.0);
                                ui.set_height(140.0);
                                ui.vertical_centered(|ui| match &entry.slot {
                                    PageSlot::Ready(data) => {
                                        let size = fit_size(
                                            data.texture.size_vec2(),
                                            Vec2::new(96.0, 112.0),
                                        );
                                        ui.add(
                                            egui::Image::new(&data.texture)
                                                .fit_to_exact_size(size),
                                        );
                                        ui.label(format!("{}", index + 1));
                                    }
                                    PageSlot::Processing | PageSlot::Reprocessing { .. } => {
                                        ui.add_space(40.0);
                                        ui.spinner();
                                        ui.label(format!("{}", index + 1));
                                    }
                                    PageSlot::Failed { .. } => {
                                        ui.add_space(34.0);
                                        ui.label(
                                            RichText::new("⚠")
                                                .size(30.0)
                                                .color(Color32::DARK_RED),
                                        );
                                        ui.label(RichText::new("Błąd").color(Color32::DARK_RED));
                                    }
                                });
                            });
                        if frame_response.response.interact(Sense::click()).clicked() {
                            clicked = Some(index);
                        }
                        ui.add_space(6.0);
                    }
                });
            });
        if let Some(index) = clicked {
            self.selected_slot = Some(index);
        }
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let mut close = false;
        let mut redetect = false;
        let mut apply = false;
        let Some(editor) = &mut self.editor else {
            return;
        };
        page_container(ui, |ui| {
            two_sided(
                ui,
                42.0,
                |ui| {
                    ui.horizontal(|ui| {
                        close = ui.button("Wróć (Esc)").clicked();
                        ui.add_space(8.0);
                        ui.heading(format!("Popraw kadr · strona {}", editor.slot_index + 1));
                    });
                },
                |ui| {
                    ui.label("Przeciągnij niebieskie punkty do narożników kartki.");
                },
            );
            ui.add_space(10.0);
            let available = Vec2::new(
                ui.available_width(),
                (ui.available_height() - 70.0).max(320.0),
            );
            let (response, painter) = ui.allocate_painter(available, Sense::hover());
            painter.rect_filled(response.rect, 12.0, Color32::from_gray(28));
            let image_size = fit_size(
                Vec2::new(editor.original.width() as f32, editor.original.height() as f32),
                available - Vec2::splat(12.0),
            );
            let image_rect = Rect::from_center_size(response.rect.center(), image_size);
            painter.image(
                editor.texture.id(),
                image_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            let positions = editor.corners.map(|point| {
                Pos2::new(
                    image_rect.left() + point.x * image_rect.width(),
                    image_rect.top() + point.y * image_rect.height(),
                )
            });
            for edge in 0..4 {
                painter.line_segment(
                    [positions[edge], positions[(edge + 1) % 4]],
                    Stroke::new(3.0, Color32::from_rgb(70, 165, 255)),
                );
            }
            for (index, position) in positions.iter().enumerate() {
                let handle_rect = Rect::from_center_size(*position, Vec2::splat(44.0));
                let drag = ui.interact(
                    handle_rect,
                    Id::new(("editor-handle", index)),
                    Sense::drag(),
                );
                if drag.dragged() {
                    let pointer = drag.interact_pointer_pos().unwrap_or(*position);
                    let normalized = CropPoint::new(
                        ((pointer.x - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0),
                        ((pointer.y - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0),
                    );
                    editor.corners[index] = constrain_editor_corner(index, normalized);
                }
                painter.circle_filled(*position, 11.0, Color32::WHITE);
                painter.circle_filled(*position, 7.0, BLUE);
            }
            ui.add_space(10.0);
            two_sided(
                ui,
                48.0,
                |ui| {
                    redetect = ui.button("Wykryj ponownie").clicked();
                },
                |ui| {
                    apply = ui
                        .add(
                            Button::new(RichText::new("Zastosuj").strong().color(Color32::WHITE))
                                .fill(BLUE)
                                .corner_radius(10.0)
                                .min_size(Vec2::new(180.0, 44.0)),
                        )
                        .clicked();
                },
            );
        });
        if redetect
            && let Some(editor) = &mut self.editor
        {
            editor.corners = crate::document::detect_document_corners(&editor.original);
        }
        if close || context.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.editor = None;
        }
        if apply {
            self.apply_editor();
        }
    }

    fn dialogs(&mut self, context: &egui::Context) {
        if self.show_new_folder {
            egui::Window::new("Nowy folder")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label("Nazwa folderu:");
                    let response = ui.add_sized(
                        [360.0, 34.0],
                        egui::TextEdit::singleline(&mut self.new_folder_name),
                    );
                    response.request_focus();
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Anuluj").clicked() {
                            self.show_new_folder = false;
                        }
                        if primary_button(ui, "Utwórz folder").clicked() {
                            match create_folder(&self.library_root, &self.new_folder_name) {
                                Ok(folder) => {
                                    self.show_new_folder = false;
                                    self.refresh_folders();
                                    self.open_folder(folder);
                                }
                                Err(error) => self.message = Some(error),
                            }
                        }
                    });
                });
        }

        if self.show_rename_folder {
            egui::Window::new("Zmień nazwę folderu")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label("Nowa nazwa:");
                    ui.add_sized(
                        [360.0, 34.0],
                        egui::TextEdit::singleline(&mut self.rename_folder_name),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Anuluj").clicked() {
                            self.show_rename_folder = false;
                        }
                        if primary_button(ui, "Zapisz nazwę").clicked()
                            && let Some(folder) = self.selected_folder.clone()
                        {
                            match rename_folder(&folder, &self.rename_folder_name) {
                                Ok(folder) => {
                                    self.selected_folder = Some(folder.clone());
                                    self.settings.last_folder = Some(folder.name.clone());
                                    let _ = save_settings(&self.settings);
                                    self.show_rename_folder = false;
                                    self.refresh_folders();
                                    self.refresh_pdfs();
                                }
                                Err(error) => self.message = Some(error),
                            }
                        }
                    });
                });
        }

        if self.show_settings {
            egui::Window::new("Ustawienia")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label(RichText::new("Folder biblioteki").strong());
                    ui.label(self.library_root.display().to_string());
                    ui.add_space(10.0);
                    if ui.button("Zmień lokalizację").clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_folder()
                    {
                        if let Err(error) = ensure_library(&path) {
                            self.message = Some(error);
                        } else {
                            self.library_root = path.clone();
                            self.settings.library_root = Some(path);
                            self.settings.last_folder = None;
                            self.selected_folder = None;
                            self.screen = Screen::Library;
                            if let Err(error) = save_settings(&self.settings) {
                                self.message = Some(error);
                            }
                            self.refresh_folders();
                        }
                    }
                    ui.add_space(12.0);
                    if primary_button(ui, "Gotowe").clicked() {
                        self.show_settings = false;
                    }
                });
        }

        if self.show_save {
            egui::Window::new("Zapisz dokument")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label(format!("Liczba stron: {}", self.slots.len()));
                    ui.label("Nazwa pliku:");
                    let response = ui.add_sized(
                        [390.0, 36.0],
                        egui::TextEdit::singleline(&mut self.filename)
                            .hint_text("np. Umowa - Kowalski"),
                    );
                    if self.save_dialog_needs_focus {
                        self.save_dialog_needs_focus = false;
                        response.request_focus();
                    }
                    ui.label(
                        RichText::new("Rozszerzenie .pdf zostanie dodane automatycznie.")
                            .small()
                            .color(Color32::GRAY),
                    );
                    ui.add_space(10.0);
                    let submitted = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let mut save_clicked = false;
                    let mut back_clicked = false;
                    ui.horizontal(|ui| {
                        back_clicked = ui.button("Wróć (Esc)").clicked();
                        save_clicked = primary_button(ui, "Zapisz PDF (Enter)").clicked();
                    });
                    if back_clicked || ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                        self.show_save = false;
                    }
                    if submitted || save_clicked {
                        self.save_current_document();
                    }
                });
        }

        if self.show_cancel_confirm {
            egui::Window::new("Anulować dokument?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label(format!(
                        "Zeskanowane strony ({}) zostaną utracone.",
                        self.slots.len()
                    ));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Wróć do skanowania").clicked() {
                            self.show_cancel_confirm = false;
                        }
                        if ui
                            .button(RichText::new("Anuluj dokument").color(Color32::DARK_RED))
                            .clicked()
                        {
                            self.show_cancel_confirm = false;
                            self.abandon_scan();
                        }
                    });
                });
        }

        if self.show_delete_confirm {
            egui::Window::new("Usunąć stronę?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    let index = self.selected_slot.map(|index| index + 1).unwrap_or(0);
                    ui.label(format!("Usunąć stronę {index}?"));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Anuluj").clicked() {
                            self.show_delete_confirm = false;
                        }
                        if ui
                            .button(RichText::new("Usuń stronę").color(Color32::DARK_RED))
                            .clicked()
                        {
                            self.show_delete_confirm = false;
                            self.delete_selected_page();
                        }
                    });
                });
        }

        if self.show_exit_confirm {
            egui::Window::new("Niezapisany dokument")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label("Zeskanowane strony nie zostały jeszcze zapisane.");
                    ui.label("Czy na pewno chcesz zamknąć program?");
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Wróć do dokumentu").clicked() {
                            self.show_exit_confirm = false;
                        }
                        if ui
                            .button(
                                RichText::new("Zamknij bez zapisywania").color(Color32::DARK_RED),
                            )
                            .clicked()
                        {
                            self.allow_exit = true;
                            self.show_exit_confirm = false;
                            context.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }

        if let Some(message) = self.message.clone() {
            egui::Window::new("Informacja")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.set_max_width(480.0);
                    ui.label(message);
                    ui.add_space(10.0);
                    if primary_button(ui, "OK").clicked() {
                        self.message = None;
                    }
                });
        }

        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.shown_at.elapsed() > Duration::from_secs(4))
        {
            self.toast = None;
        }
        if let Some(toast) = &self.toast {
            egui::Area::new(Id::new("zapis-toast"))
                .anchor(egui::Align2::RIGHT_BOTTOM, Vec2::new(-24.0, -24.0))
                .show(context, |ui| {
                    Frame::new()
                        .fill(Color32::from_rgb(34, 120, 62))
                        .corner_radius(10.0)
                        .inner_margin(Margin::symmetric(16, 10))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&toast.text).color(Color32::WHITE).size(16.0));
                        });
                });
            context.request_repaint_after(Duration::from_millis(250));
        }
    }
}

impl eframe::App for DocumentScannerApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        let close_requested = context.input(|input| input.viewport().close_requested());
        let has_unsaved_scan = !self.slots.is_empty();
        if close_requested && has_unsaved_scan && !self.allow_exit {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_exit_confirm = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        let dialog_open = self.show_save
            || self.show_cancel_confirm
            || self.show_delete_confirm
            || self.show_settings
            || self.show_new_folder
            || self.show_rename_folder
            || self.show_exit_confirm
            || self.message.is_some()
            || self.editor.is_some();
        if self.screen == Screen::ScanHub && !dialog_open {
            let (space, enter) = context.input(|input| {
                (
                    input.key_pressed(egui::Key::Space),
                    input.key_pressed(egui::Key::Enter),
                )
            });
            if space {
                self.capture(true);
            }
            if enter && self.can_save() {
                self.save_dialog_needs_focus = true;
                self.show_save = true;
            }
        }
        Frame::new().fill(BACKGROUND).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            self.top_bar(ui);
            match self.screen {
                Screen::Library => self.library_ui(ui),
                Screen::Folder => self.folder_ui(ui),
                Screen::ScanHub => self.scan_hub_ui(ui, &context),
            }
        });
        self.dialogs(&context);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_camera();
        let _ = save_settings(&self.settings);
    }
}

fn configure_style(context: &egui::Context) {
    context.set_theme(egui::Theme::Light);
    let mut style = (*context.style_of(egui::Theme::Light)).clone();
    style.spacing.button_padding = Vec2::new(16.0, 10.0);
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.visuals = egui::Visuals::light();
    style.visuals.widgets.active.bg_fill = BLUE;
    style.visuals.widgets.hovered.bg_fill = PALE_BLUE;
    style.visuals.window_corner_radius = CornerRadius::same(12);
    style
        .text_styles
        .insert(egui::TextStyle::Heading, FontId::proportional(24.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(16.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(16.0));
    context.set_style_of(egui::Theme::Light, style);
}

fn page_container(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .inner_margin(Margin::symmetric(28, 22))
        .show(ui, |ui| add_contents(ui));
}

fn two_sided<Left, Right>(
    ui: &mut egui::Ui,
    height: f32,
    add_left: impl FnOnce(&mut egui::Ui) -> Left,
    add_right: impl FnOnce(&mut egui::Ui) -> Right,
) -> (Left, Right) {
    let min = ui.available_rect_before_wrap().min;
    let width = (ui.clip_rect().right() - min.x).max(0.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let id = ui.next_auto_id();
    let mut left_ui = ui.new_child(
        UiBuilder::new()
            .id_salt((id, "left"))
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let mut right_ui = ui.new_child(
        UiBuilder::new()
            .id_salt((id, "right"))
            .max_rect(rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    let left = add_left(&mut left_ui);
    let right = add_right(&mut right_ui);
    (left, right)
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        Button::new(RichText::new(text).strong().color(Color32::WHITE))
            .fill(BLUE)
            .corner_radius(9.0),
    )
}

fn empty_card(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, Color32::from_gray(222)))
        .corner_radius(12.0)
        .inner_margin(36)
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(20.0).strong());
                ui.label(RichText::new(subtitle).color(Color32::GRAY));
            });
        });
}

fn rgb_to_color_image(image: &RgbImage) -> ColorImage {
    ColorImage::from_rgb(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}

fn fit_size(source: Vec2, bounds: Vec2) -> Vec2 {
    if source.x <= 0.0 || source.y <= 0.0 {
        return Vec2::ZERO;
    }
    let scale = (bounds.x / source.x).min(bounds.y / source.y).max(0.0);
    source * scale
}

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBeep(utype: u32) -> i32;
}

fn fallback_editor_corners() -> [CropPoint; 4] {
    [
        CropPoint::new(0.06, 0.06),
        CropPoint::new(0.94, 0.06),
        CropPoint::new(0.94, 0.94),
        CropPoint::new(0.06, 0.94),
    ]
}

fn constrain_editor_corner(index: usize, point: CropPoint) -> CropPoint {
    match index {
        0 => CropPoint::new(point.x.min(0.49), point.y.min(0.49)),
        1 => CropPoint::new(point.x.max(0.51), point.y.min(0.49)),
        2 => CropPoint::new(point.x.max(0.51), point.y.max(0.51)),
        3 => CropPoint::new(point.x.min(0.49), point.y.max(0.51)),
        _ => point,
    }
}

fn beep() {
    #[cfg(windows)]
    unsafe {
        let _ = MessageBeep(0);
    }
}

fn polish_page_count(count: usize) -> String {
    let word = if count == 1 {
        "strona"
    } else if (2..=4).contains(&(count % 10)) && !(12..=14).contains(&(count % 100)) {
        "strony"
    } else {
        "stron"
    };
    format!("{count} {word}")
}
