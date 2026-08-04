use crate::autocapture::{AutoCapture, FeedResult};
use crate::camera::{CameraController, CameraEvent};
use crate::document::{
    CropPoint, ScannedPage, extract_pdf_pages, page_from_jpeg_bytes, pages_from_jpeg_bytes,
    render_pdf, rotate_page_by_quarter_turns, rotate_page_clockwise,
};
use crate::overlay::OverlayDetector;
use crate::pipeline::{PipelineEvent, ProcessingPipeline};
use crate::review_viewport::{PageTextureKey, ReviewViewport};
use crate::session::{RecoveredSession, SessionStore};
use crate::storage::{
    FolderInfo, PdfInfo, Settings, create_folder, default_library_root, ensure_library,
    list_folders, list_pdfs, load_settings, normalized_pdf_stem, rename_folder, save_settings,
    unique_pdf_path,
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
const STRIP_HEIGHT: f32 = 160.0;

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
    quarter_turns: u8,
    texture: TextureHandle,
}

enum PageSlot {
    Processing,
    Ready(Box<PageData>),
    Failed {
        original_jpeg: Vec<u8>,
        error: String,
        quarter_turns: u8,
    },
    Reprocessing {
        original_jpeg: Vec<u8>,
        quarter_turns: u8,
        previous: Option<Box<PageData>>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SaveMode {
    New,
    Update,
    Copy,
}

struct SavePlan {
    path: PathBuf,
    mode: SaveMode,
}

struct EditorState {
    slot_index: usize,
    original: RgbImage,
    texture: TextureHandle,
    corners: [CropPoint; 4],
}

struct SlotEntry {
    id: u64,
    revision: u64,
    slot: PageSlot,
}

struct Toast {
    text: String,
    shown_at: Instant,
    pdf_path: Option<PathBuf>,
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
    capture_flash: Option<Instant>,
    last_slot_count: usize,
    session: Option<SessionStore>,
    session_broken: bool,
    recovered: Option<RecoveredSession>,
    show_restore: bool,
    filename: String,
    editing_target: Option<PathBuf>,
    editing_source_fingerprint: Option<u64>,
    edit_dirty: bool,
    reviewing: bool,
    review_viewport: ReviewViewport,
    toast: Option<Toast>,

    show_new_folder: bool,
    new_folder_name: String,
    show_rename_folder: bool,
    rename_folder_name: String,
    show_settings: bool,
    save_dialog_needs_focus: bool,
    show_cancel_confirm: bool,
    delete_page_target: Option<u64>,
    delete_pdf_target: Option<PdfInfo>,
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
            capture_flash: None,
            last_slot_count: 0,
            session: None,
            session_broken: false,
            recovered: None,
            show_restore: false,
            filename: String::new(),
            editing_target: None,
            editing_source_fingerprint: None,
            edit_dirty: false,
            reviewing: false,
            review_viewport: ReviewViewport::default(),
            toast: None,
            show_new_folder: false,
            new_folder_name: String::new(),
            show_rename_folder: false,
            rename_folder_name: String::new(),
            show_settings: false,
            save_dialog_needs_focus: false,
            show_cancel_confirm: false,
            delete_page_target: None,
            delete_pdf_target: None,
            show_exit_confirm: false,
            allow_exit: false,
            message: None,
        };
        if let Err(error) = ensure_library(&app.library_root) {
            app.message = Some(error);
        }
        app.refresh_folders();
        app.restore_last_folder();
        app.session = SessionStore::open_default();
        if let Some(recovered) = app.session.as_ref().and_then(SessionStore::load_existing) {
            app.recovered = Some(recovered);
            app.show_restore = true;
        }
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
        self.filename = self
            .selected_folder
            .as_ref()
            .map(|folder| folder.name.clone())
            .unwrap_or_default();
        self.editing_target = None;
        self.editing_source_fingerprint = None;
        self.edit_dirty = false;
        self.reviewing = false;
        self.review_viewport.clear();
        self.pipeline = Some(ProcessingPipeline::start());
        self.overlay = Some(OverlayDetector::start());
        self.autocapture = AutoCapture::new();
        if self.settings.auto_capture == Some(false) {
            self.autocapture.set_enabled(false);
        }
        self.session_broken = false;
        if let (Some(session), Some(folder)) = (&self.session, &self.selected_folder)
            && let Err(error) = session.begin(&folder.path)
        {
            self.session_broken = true;
            self.toast = Some(Toast {
                text: format!("Kopia sesji wyłączona: {error}"),
                shown_at: Instant::now(),
                pdf_path: None,
            });
        }
        self.start_camera();
    }

    fn session_write_page(
        &mut self,
        id: u64,
        jpeg: &[u8],
        original_jpeg: &[u8],
        corners: [CropPoint; 4],
        quarter_turns: u8,
    ) {
        if self.session_broken {
            return;
        }
        if let Some(session) = &self.session
            && let Err(error) = session.write_page(id, jpeg, original_jpeg, corners, quarter_turns)
        {
            self.session_broken = true;
            self.toast = Some(Toast {
                text: format!("Kopia sesji wyłączona: {error}"),
                shown_at: Instant::now(),
                pdf_path: None,
            });
        }
    }

    fn session_sync_order(&mut self) {
        if self.session_broken {
            return;
        }
        let ids: Vec<u64> = self.slots.iter().map(|entry| entry.id).collect();
        if let Some(session) = &self.session
            && let Err(error) = session.set_order(&ids)
        {
            self.session_broken = true;
            self.toast = Some(Toast {
                text: format!("Kopia sesji wyłączona: {error}"),
                shown_at: Instant::now(),
                pdf_path: None,
            });
        }
    }

    fn session_clear(&mut self) {
        if let Some(session) = &self.session {
            let _ = session.clear();
        }
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
        let live_preview = !self.reviewing && self.editor.is_none();
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
                    if live_preview {
                        self.update_preview_texture(context, &image);
                        if let Some(overlay) = &self.overlay {
                            overlay.submit(image.clone());
                        }
                        self.pending_preview = Some(image);
                    }
                }
                CameraEvent::Error(error) => {
                    self.camera_ready = false;
                    self.preview_texture = None;
                    self.camera_status = error;
                }
            }
        }
        if self.screen == Screen::ScanHub && live_preview {
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

    fn capture(&mut self, manual: bool) -> bool {
        let Some(frame) = self
            .camera
            .as_ref()
            .and_then(CameraController::latest_full_image)
        else {
            if manual {
                self.message = Some("Poczekaj, aż pojawi się obraz z kamery.".to_owned());
            }
            return false;
        };
        let Some(pipeline) = &self.pipeline else {
            return false;
        };
        let id = self.next_page_id;
        if pipeline.try_submit(id, frame) {
            self.next_page_id += 1;
            self.slots.push(SlotEntry {
                id,
                revision: 0,
                slot: PageSlot::Processing,
            });
            self.pending_jobs += 1;
            if self.editing_target.is_some() {
                self.edit_dirty = true;
            }
            if manual {
                self.autocapture.note_manual_capture();
            } else {
                self.autocapture.note_capture_accepted();
            }
            self.capture_flash = Some(Instant::now());
            beep();
            true
        } else if manual {
            self.message = Some("Poczekaj — przetwarzanie poprzednich stron…".to_owned());
            false
        } else {
            false
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
                    let Some(index) = self.slots.iter().position(|entry| {
                        entry.id == id && matches!(entry.slot, PageSlot::Processing)
                    }) else {
                        continue;
                    };
                    self.session_write_page(id, &page.jpeg, &original_jpeg, corners, 0);
                    let texture = context.load_texture(
                        format!("strona-{id}"),
                        rgb_to_color_image(&page.review_image),
                        TextureOptions::LINEAR,
                    );
                    self.slots[index].slot = PageSlot::Ready(Box::new(PageData {
                        page,
                        original_jpeg,
                        corners,
                        quarter_turns: 0,
                        texture,
                    }));
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
                            quarter_turns: 0,
                        };
                    }
                }
                PipelineEvent::ReprocessDone { id, page, corners } => {
                    let mut persisted = None;
                    let mut feedback = None;
                    if let Some(entry) = self.slots.iter_mut().find(|entry| entry.id == id) {
                        let current = std::mem::replace(&mut entry.slot, PageSlot::Processing);
                        let (original_jpeg, quarter_turns, previous) = match current {
                            PageSlot::Reprocessing {
                                original_jpeg,
                                quarter_turns,
                                previous,
                            } => (original_jpeg, quarter_turns, previous),
                            other => {
                                entry.slot = other;
                                continue;
                            }
                        };
                        let processed = if quarter_turns > 0 {
                            rotate_page_by_quarter_turns(&page, quarter_turns)
                        } else {
                            Ok(page)
                        };
                        match processed {
                            Ok(page) => {
                                let texture = context.load_texture(
                                    format!("strona-{id}-kadr-{}", entry.revision + 1),
                                    rgb_to_color_image(&page.review_image),
                                    TextureOptions::LINEAR,
                                );
                                let persisted_jpeg = page.jpeg.clone();
                                entry.revision += 1;
                                entry.slot = PageSlot::Ready(Box::new(PageData {
                                    page,
                                    original_jpeg: original_jpeg.clone(),
                                    corners,
                                    quarter_turns,
                                    texture,
                                }));
                                persisted = Some((persisted_jpeg, original_jpeg, quarter_turns));
                            }
                            Err(error) => {
                                feedback =
                                    Some(reprocess_failure_message(previous.is_some(), &error));
                                entry.slot = rollback_reprocessing(
                                    previous,
                                    original_jpeg,
                                    quarter_turns,
                                    error,
                                );
                            }
                        }
                    }
                    if let Some(message) = feedback {
                        self.message = Some(message);
                    }
                    if let Some((jpeg, original_jpeg, quarter_turns)) = persisted {
                        self.session_write_page(id, &jpeg, &original_jpeg, corners, quarter_turns);
                    }
                }
                PipelineEvent::ReprocessFailed { id, error } => {
                    let mut feedback = None;
                    if let Some(entry) = self.slots.iter_mut().find(|entry| entry.id == id) {
                        let current = std::mem::replace(&mut entry.slot, PageSlot::Processing);
                        let (original_jpeg, quarter_turns, previous) = match current {
                            PageSlot::Reprocessing {
                                original_jpeg,
                                quarter_turns,
                                previous,
                            } => (original_jpeg, quarter_turns, previous),
                            other => {
                                entry.slot = other;
                                continue;
                            }
                        };
                        feedback = Some(reprocess_failure_message(previous.is_some(), &error));
                        entry.slot =
                            rollback_reprocessing(previous, original_jpeg, quarter_turns, error);
                    }
                    if let Some(message) = feedback {
                        self.message = Some(message);
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

    fn can_submit_save(&self) -> bool {
        self.can_save() && self.resolve_save_target().is_ok()
    }

    fn has_active_workflow(&self) -> bool {
        has_active_workflow_state(
            self.screen,
            self.pipeline.is_some(),
            !self.slots.is_empty(),
            self.pending_jobs > 0,
            self.editing_target.is_some(),
        )
    }

    fn has_blocking_dialog(&self) -> bool {
        self.show_restore
            || self.show_new_folder
            || self.show_rename_folder
            || self.show_settings
            || self.show_cancel_confirm
            || self.delete_page_target.is_some()
            || self.show_exit_confirm
            || self.delete_pdf_target.is_some()
            || self.message.is_some()
    }

    fn resolve_save_target(&self) -> Result<SavePlan, String> {
        let folder = self
            .selected_folder
            .as_ref()
            .ok_or_else(|| "Najpierw wybierz folder docelowy.".to_owned())?;
        let requested_stem = normalized_pdf_stem(&self.filename)?;
        if let Some(target) = &self.editing_target {
            let target_stem = target
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            if requested_stem.to_lowercase() == target_stem.to_lowercase() {
                return Ok(SavePlan {
                    path: target.clone(),
                    mode: SaveMode::Update,
                });
            }
            return unique_pdf_path(&folder.path, &requested_stem).map(|path| SavePlan {
                path,
                mode: SaveMode::Copy,
            });
        }
        unique_pdf_path(&folder.path, &requested_stem).map(|path| SavePlan {
            path,
            mode: SaveMode::New,
        })
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
                entry.revision += 1;
                data.texture = context.load_texture(
                    format!("strona-{}-obrot-{}", entry.id, entry.revision),
                    rgb_to_color_image(&rotated.review_image),
                    TextureOptions::LINEAR,
                );
                data.page = rotated;
                data.quarter_turns = (data.quarter_turns + 1) % 4;
                if self.editing_target.is_some() {
                    self.edit_dirty = true;
                }
            }
            Err(error) => {
                self.message = Some(error);
                return;
            }
        }
        let written = match self.slots.get(index) {
            Some(entry) => match &entry.slot {
                PageSlot::Ready(data) => Some((
                    entry.id,
                    data.page.jpeg.clone(),
                    data.original_jpeg.clone(),
                    data.corners,
                    data.quarter_turns,
                )),
                _ => None,
            },
            None => None,
        };
        if let Some((id, jpeg, original_jpeg, corners, quarter_turns)) = written {
            self.session_write_page(id, &jpeg, &original_jpeg, corners, quarter_turns);
        }
        self.review_viewport.invalidate();
    }

    fn delete_selected_page(&mut self) {
        let Some(index) = self.selected_slot else {
            return;
        };
        if index >= self.slots.len() {
            self.selected_slot = None;
            return;
        }
        let removed_id = self.slots[index].id;
        self.slots.remove(index);
        if self.editing_target.is_some() {
            self.edit_dirty = true;
        }
        self.selected_slot = if self.slots.is_empty() {
            None
        } else {
            Some(index.min(self.slots.len() - 1))
        };
        if let Some(session) = &self.session {
            let _ = session.remove_page(removed_id);
        }
        self.session_sync_order();
    }

    fn request_delete_selected_page(&mut self) {
        self.delete_page_target = self
            .selected_slot
            .and_then(|index| self.slots.get(index))
            .map(|entry| entry.id);
    }

    fn delete_page_by_id(&mut self, id: u64) {
        let Some(index) = self.slots.iter().position(|entry| entry.id == id) else {
            return;
        };
        self.selected_slot = Some(index);
        self.delete_selected_page();
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
        if self.editing_target.is_some() {
            self.edit_dirty = true;
        }
        self.session_sync_order();
    }

    fn save_current_document(&mut self) {
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
        let plan = match self.resolve_save_target() {
            Ok(plan) => plan,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        let pdf_bytes = match render_pdf(&pages) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        drop(pages);

        let SavePlan {
            path: planned_path,
            mode,
        } = plan;
        let save_result = match mode {
            SaveMode::Update => self.commit_pdf_update(planned_path, &pdf_bytes),
            SaveMode::New | SaveMode::Copy => self.commit_pdf_new(planned_path, &pdf_bytes),
        };
        match save_result {
            Ok(path) => {
                let verb = match mode {
                    SaveMode::New => "Zapisano",
                    SaveMode::Update => "Zaktualizowano",
                    SaveMode::Copy => "Zapisano kopię",
                };
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.toast = Some(Toast {
                    text: format!("{verb}: {name}"),
                    shown_at: Instant::now(),
                    pdf_path: Some(path.clone()),
                });
                self.filename.clear();
                self.refresh_folders();
                self.abandon_scan();
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn commit_pdf_update(&self, path: PathBuf, bytes: &[u8]) -> Result<PathBuf, String> {
        let prepared = crate::atomic_file::prepare(&path, bytes).map_err(pdf_io_error)?;
        let source = self
            .editing_target
            .as_ref()
            .ok_or_else(|| "Brak pliku źródłowego do aktualizacji.".to_owned())?;
        let current_fingerprint = file_fingerprint(source)?;
        if self.editing_source_fingerprint != Some(current_fingerprint) {
            return Err(
                "Plik źródłowy zmienił się poza programem. Zapisz dokument pod inną nazwą, aby nie nadpisać cudzych zmian."
                    .to_owned(),
            );
        }
        prepared.commit_replace(&path).map_err(pdf_io_error)?;
        Ok(path)
    }

    fn commit_pdf_new(&self, mut path: PathBuf, bytes: &[u8]) -> Result<PathBuf, String> {
        let folder = self
            .selected_folder
            .as_ref()
            .ok_or_else(|| "Najpierw wybierz folder docelowy.".to_owned())?;
        let stem = normalized_pdf_stem(&self.filename)?;
        for _ in 0..100 {
            let prepared = crate::atomic_file::prepare(&path, bytes).map_err(pdf_io_error)?;
            match prepared.commit_new(&path) {
                Ok(()) => return Ok(path),
                Err(_) if path.exists() => {
                    path = unique_pdf_path(&folder.path, &stem)?;
                }
                Err(error) => return Err(pdf_io_error(error)),
            }
        }
        Err("Nie można utworzyć unikalnej nazwy pliku.".to_owned())
    }

    fn restore_recovered_session(&mut self, context: &egui::Context) {
        let Some(recovered) = self.recovered.take() else {
            return;
        };
        let skipped_pages = recovered.skipped_pages;
        let folder_name = recovered
            .folder_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.selected_folder = Some(FolderInfo {
            name: folder_name.clone(),
            path: recovered.folder_path.clone(),
            pdf_count: 0,
        });
        self.slots.clear();
        self.selected_slot = None;
        self.pending_jobs = 0;
        self.filename = folder_name.clone();
        self.review_viewport.clear();
        self.pipeline = Some(ProcessingPipeline::start());
        self.overlay = Some(OverlayDetector::start());
        self.autocapture = AutoCapture::new();
        if self.settings.auto_capture == Some(false) {
            self.autocapture.set_enabled(false);
        }
        self.session_broken = false;
        let mut max_id = 0;
        for recovered_page in recovered.pages {
            let id = recovered_page.id;
            let processed_jpeg = recovered_page.jpeg;
            let original_jpeg = recovered_page.original_jpeg.unwrap_or_default();
            let corners = recovered_page
                .corners
                .unwrap_or_else(full_frame_editor_corners);
            let quarter_turns = recovered_page.quarter_turns;
            max_id = max_id.max(id);
            let recovered_page = processed_jpeg
                .ok_or_else(|| "Brak przetworzonego obrazu strony.".to_owned())
                .and_then(page_from_jpeg_bytes);
            match recovered_page {
                Ok(page) => {
                    let texture = context.load_texture(
                        format!("strona-{id}"),
                        rgb_to_color_image(&page.review_image),
                        TextureOptions::LINEAR,
                    );
                    self.slots.push(SlotEntry {
                        id,
                        revision: 0,
                        slot: PageSlot::Ready(Box::new(PageData {
                            page,
                            original_jpeg,
                            corners,
                            quarter_turns,
                            texture,
                        })),
                    });
                }
                Err(error) => {
                    self.slots.push(SlotEntry {
                        id,
                        revision: 0,
                        slot: PageSlot::Failed {
                            original_jpeg,
                            error,
                            quarter_turns,
                        },
                    });
                }
            }
        }
        self.next_page_id = max_id + 1;
        self.refresh_pdfs();
        self.start_camera();
        if skipped_pages > 0 {
            self.message = Some(format!(
                "Nie udało się odzyskać {skipped_pages} stron bez obrazu ani oryginału. Pozostałe strony zachowano."
            ));
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
        let quarter_turns = match &entry.slot {
            PageSlot::Ready(data) => data.quarter_turns,
            PageSlot::Failed { quarter_turns, .. } => *quarter_turns,
            _ => return,
        };
        if pipeline.submit_reprocess(entry.id, Arc::new(editor.original), editor.corners) {
            let current = std::mem::replace(&mut entry.slot, PageSlot::Processing);
            let (original_jpeg, previous) = match current {
                PageSlot::Ready(mut data) => {
                    let original_jpeg = std::mem::take(&mut data.original_jpeg);
                    (original_jpeg, Some(data))
                }
                PageSlot::Failed { original_jpeg, .. } => (original_jpeg, None),
                other => {
                    entry.slot = other;
                    return;
                }
            };
            entry.slot = PageSlot::Reprocessing {
                original_jpeg,
                quarter_turns,
                previous,
            };
            self.pending_jobs += 1;
            if self.editing_target.is_some() {
                self.edit_dirty = true;
            }
            self.review_viewport.invalidate();
        } else {
            self.message = Some("Kolejka przetwarzania jest pełna. Spróbuj ponownie.".to_owned());
        }
    }

    fn request_cancel_scan(&mut self) {
        if can_leave_scan_without_confirmation(
            self.slots.is_empty(),
            self.editing_target.is_some(),
            self.edit_dirty,
        ) {
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
        self.editing_target = None;
        self.editing_source_fingerprint = None;
        self.edit_dirty = false;
        self.reviewing = false;
        self.review_viewport.clear();
        self.editor = None;
        self.delete_page_target = None;
        self.pending_preview = None;
        self.filename.clear();
        self.session_clear();
        self.screen = Screen::Folder;
        self.refresh_pdfs();
    }

    fn open_pdf_for_edit(&mut self, pdf: &PdfInfo, context: &egui::Context) {
        let source_fingerprint = match file_fingerprint(&pdf.path) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        let jpegs = match extract_pdf_pages(&pdf.path) {
            Ok(jpegs) => jpegs,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        let pages = match pages_from_jpeg_bytes(jpegs) {
            Ok(pages) => pages,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        let stem = pdf
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| pdf.name.clone());
        let target = pdf.path.clone();
        self.begin_scan();
        for page in pages {
            let id = self.next_page_id;
            self.next_page_id += 1;
            // An imported PDF only contains the already processed page, not the
            // original camera frame. Treating it as a camera original would
            // apply perspective correction and JPEG compression a second time.
            let original_jpeg = Vec::new();
            let corners = full_frame_editor_corners();
            self.session_write_page(id, &page.jpeg, &original_jpeg, corners, 0);
            let texture = context.load_texture(
                format!("strona-{id}"),
                rgb_to_color_image(&page.review_image),
                TextureOptions::LINEAR,
            );
            self.slots.push(SlotEntry {
                id,
                revision: 0,
                slot: PageSlot::Ready(Box::new(PageData {
                    page,
                    original_jpeg,
                    corners,
                    quarter_turns: 0,
                    texture,
                })),
            });
        }
        self.filename = stem;
        self.editing_target = Some(target);
        self.editing_source_fingerprint = Some(source_fingerprint);
        self.edit_dirty = false;
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
                        let settings =
                            ui.add_enabled(!self.has_active_workflow(), Button::new("Ustawienia"));
                        if settings.clicked() {
                            self.show_settings = true;
                        }
                        settings.on_disabled_hover_text(
                            "Najpierw zapisz albo anuluj bieżący dokument.",
                        );
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
                let mut open_target: Option<PathBuf> = None;
                let mut delete_target: Option<PdfInfo> = None;
                let mut edit_target: Option<PdfInfo> = None;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for pdf in &self.pdfs {
                        Frame::new()
                            .fill(Color32::WHITE)
                            .corner_radius(10.0)
                            .stroke(Stroke::new(1.0, Color32::from_gray(222)))
                            .inner_margin(Margin::symmetric(18, 14))
                            .show(ui, |ui| {
                                let (_, (open_pdf, edit_pdf, delete_pdf)) = two_sided(
                                    ui,
                                    38.0,
                                    |ui| {
                                        ui.label(RichText::new(&pdf.name).size(16.0));
                                    },
                                    |ui| {
                                        let open_pdf = ui.button("Otwórz PDF").clicked();
                                        let edit_pdf = ui.button("Edytuj").clicked();
                                        let delete_pdf = ui
                                            .button(RichText::new("Usuń").color(Color32::DARK_RED))
                                            .clicked();
                                        (open_pdf, edit_pdf, delete_pdf)
                                    },
                                );
                                if open_pdf {
                                    open_target = Some(pdf.path.clone());
                                }
                                if edit_pdf {
                                    edit_target = Some(pdf.clone());
                                }
                                if delete_pdf {
                                    delete_target = Some(pdf.clone());
                                }
                            });
                        ui.add_space(8.0);
                    }
                });
                if let Some(path) = open_target
                    && let Err(error) = open::that_detached(&path)
                {
                    self.message = Some(format!("Nie można otworzyć PDF: {error}"));
                }
                if delete_target.is_some() {
                    self.delete_pdf_target = delete_target;
                }
                if let Some(pdf) = edit_target {
                    self.open_pdf_for_edit(&pdf, ui.ctx());
                }
            }
        });
    }

    fn scan_hub_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        self.poll_camera(context);
        self.poll_pipeline(context);
        let dialog_open = self.reviewing || self.editor.is_some() || self.has_blocking_dialog();
        let document_present = self
            .overlay
            .as_ref()
            .and_then(OverlayDetector::latest)
            .is_some_and(|result| result.confident);
        if let Some(preview) = self.pending_preview.take()
            && self.camera_ready
            && !dialog_open
            && self.editor.is_none()
            && self
                .autocapture
                .feed(&preview, Instant::now(), document_present)
                == FeedResult::Trigger
        {
            self.capture(false);
        }
        if self.editor.is_some() {
            self.editor_ui(ui, context);
            return;
        }
        if self.reviewing {
            self.review_ui(ui, context);
            return;
        }
        page_container(ui, |ui| {
            let camera_ready = self.camera_ready;
            let camera_status = self.camera_status.clone();
            let page_count_text = polish_page_count(self.slots.len());
            let slots_empty = self.slots.is_empty();
            let editing_pdf = self.editing_target.is_some();
            let can_return_to_folder =
                can_leave_scan_without_confirmation(slots_empty, editing_pdf, self.edit_dirty);
            let ((back, cancel), ()) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut back = false;
                    let mut cancel = false;
                    ui.horizontal(|ui| {
                        back = ui
                            .add_enabled(can_return_to_folder, Button::new("Wróć do folderu"))
                            .clicked();
                        let cancel_label = if editing_pdf {
                            "Anuluj edycję"
                        } else {
                            "Anuluj dokument"
                        };
                        cancel = ui
                            .button(RichText::new(cancel_label).color(Color32::DARK_RED))
                            .clicked();
                        ui.add_space(10.0);
                        ui.heading(format!("Skanowanie · {page_count_text}"));
                    });
                    (back, cancel)
                },
                |ui| {
                    let auto_on = self.autocapture.enabled();
                    let toggle_label = if auto_on { "Auto: WŁ" } else { "Auto: WYŁ" };
                    let toggle =
                        Button::new(RichText::new(toggle_label).strong().color(if auto_on {
                            Color32::WHITE
                        } else {
                            BLUE_DARK
                        }))
                        .fill(if auto_on { BLUE } else { Color32::WHITE })
                        .stroke(Stroke::new(1.0, BLUE))
                        .corner_radius(9.0);
                    if ui.add(toggle).clicked() {
                        self.autocapture.set_enabled(!auto_on);
                        self.settings.auto_capture = Some(!auto_on);
                        let _ = save_settings(&self.settings);
                    }
                },
            );
            if back {
                self.abandon_scan();
                return;
            }
            if cancel {
                self.request_cancel_scan();
                return;
            }
            let status_color = if camera_ready {
                Color32::DARK_GREEN
            } else {
                Color32::DARK_GRAY
            };
            ui.allocate_ui(Vec2::new(ui.available_width(), 22.0), |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(camera_status).color(status_color).size(14.0));
                    if camera_ready {
                        ui.label(RichText::new("·").color(Color32::GRAY).size(14.0));
                        ui.label(
                            RichText::new(self.autocapture.hint())
                                .color(Color32::from_gray(95))
                                .size(14.0),
                        );
                    }
                });
            });
            ui.add_space(6.0);

            let controls_min = ui.available_rect_before_wrap().min;
            let controls_width = (ui.max_rect().right() - controls_min.x).max(0.0);
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
                "Przejrzyj i zapisz (Enter)".to_owned()
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
                self.reviewing = true;
            }
            if !self.camera_ready && controls_ui.button("Spróbuj ponownie").clicked() {
                self.start_camera();
            }
            ui.add_space(8.0);

            if self
                .selected_slot
                .is_some_and(|index| index >= self.slots.len())
            {
                self.selected_slot = None;
            }
            let selection = self.selected_slot;
            ui.horizontal(|ui| {
                match selection {
                    Some(index) => {
                        ui.label(
                            RichText::new(format!("Strona {} z {}", index + 1, self.slots.len()))
                                .strong(),
                        );
                    }
                    None => {
                        ui.label(
                            RichText::new("Kliknij miniaturę, aby zaznaczyć stronę")
                                .color(Color32::GRAY),
                        );
                    }
                }
                let can_left = selection.is_some_and(|index| index > 0);
                let can_right = selection.is_some_and(|index| index + 1 < self.slots.len());
                if ui.add_enabled(can_left, Button::new("← W lewo")).clicked() {
                    self.move_selected_page(-1);
                }
                if ui
                    .add_enabled(can_right, Button::new("W prawo →"))
                    .clicked()
                {
                    self.move_selected_page(1);
                }
                let rotatable = selection.is_some_and(|index| {
                    matches!(
                        self.slots.get(index).map(|entry| &entry.slot),
                        Some(PageSlot::Ready(_))
                    )
                });
                if ui.add_enabled(rotatable, Button::new("Obróć")).clicked() {
                    self.rotate_selected_page(context);
                }
                let editable = selection.is_some_and(|index| {
                    slot_has_editable_original(self.slots.get(index).map(|entry| &entry.slot))
                });
                if ui
                    .add_enabled(editable, Button::new("Popraw kadr"))
                    .clicked()
                    && let Some(index) = selection
                {
                    self.open_editor(index, context);
                }
                if ui
                    .add_enabled(
                        selection.is_some(),
                        Button::new(RichText::new("Usuń stronę").color(Color32::DARK_RED)),
                    )
                    .clicked()
                {
                    self.request_delete_selected_page();
                }
                if ui
                    .add_enabled(selection.is_some(), Button::new("Odznacz"))
                    .clicked()
                {
                    self.selected_slot = None;
                }
            });
            ui.add_space(6.0);

            let preview_min = ui.available_rect_before_wrap().min;
            let preview_max = Pos2::new(
                ui.max_rect().right(),
                (ui.max_rect().bottom() - STRIP_HEIGHT - 10.0).max(preview_min.y + 80.0),
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
                    if let Some(result) = self.overlay.as_ref().and_then(OverlayDetector::latest)
                        && result.confident
                    {
                        let points = result.corners.map(|corner| {
                            Pos2::new(
                                draw_rect.left() + corner.x * draw_rect.width(),
                                draw_rect.top() + corner.y * draw_rect.height(),
                            )
                        });
                        let flash = self
                            .capture_flash
                            .is_some_and(|at| at.elapsed() < Duration::from_millis(350));
                        let base_color = if flash {
                            Color32::from_rgb(60, 210, 110)
                        } else {
                            Color32::from_rgb(70, 165, 255)
                        };
                        let base_width = if flash { 6.0 } else { 2.0 };
                        for edge in 0..4 {
                            painter.line_segment(
                                [points[edge], points[(edge + 1) % 4]],
                                Stroke::new(base_width, base_color),
                            );
                        }
                        let progress = self.autocapture.settle_progress(Instant::now());
                        if !flash && progress > 0.0 {
                            draw_progress_outline(&painter, &points, progress);
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
            let strip_max = Pos2::new(ui.max_rect().right(), ui.max_rect().bottom());
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
        let tile_height = (ui.available_height() - 12.0).clamp(104.0, 140.0);
        let image_height = (tile_height - 28.0).max(76.0);
        if self.slots.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space((tile_height * 0.35).max(24.0));
                ui.label(
                    RichText::new("Naciśnij Spację lub połóż stronę — miniatury pojawią się tutaj")
                        .color(Color32::GRAY),
                );
            });
            self.last_slot_count = 0;
            return;
        }
        let strip_grew = self.slots.len() > self.last_slot_count;
        let last_index = self.slots.len() - 1;
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
                                ui.set_height(tile_height);
                                ui.vertical_centered(|ui| match &entry.slot {
                                    PageSlot::Ready(data) => {
                                        let size = fit_size(
                                            data.texture.size_vec2(),
                                            Vec2::new(96.0, image_height),
                                        );
                                        ui.add(
                                            egui::Image::new(&data.texture).fit_to_exact_size(size),
                                        );
                                        ui.label(format!("{}", index + 1));
                                    }
                                    PageSlot::Processing | PageSlot::Reprocessing { .. } => {
                                        ui.add_space(40.0);
                                        ui.spinner();
                                        ui.label(format!("{}", index + 1));
                                    }
                                    PageSlot::Failed { error, .. } => {
                                        ui.add_space((tile_height * 0.22).max(20.0));
                                        ui.label(
                                            RichText::new("⚠").size(30.0).color(Color32::DARK_RED),
                                        );
                                        ui.label(
                                            RichText::new(format!("{} · Błąd", index + 1))
                                                .color(Color32::DARK_RED),
                                        )
                                        .on_hover_text(error);
                                    }
                                });
                            });
                        if frame_response.response.interact(Sense::click()).clicked() {
                            clicked = Some(index);
                        }
                        if strip_grew && index == last_index {
                            frame_response.response.scroll_to_me(Some(Align::Max));
                        }
                        ui.add_space(6.0);
                    }
                });
            });
        self.last_slot_count = self.slots.len();
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
                Vec2::new(
                    editor.original.width() as f32,
                    editor.original.height() as f32,
                ),
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
        if redetect && let Some(editor) = &mut self.editor {
            editor.corners = crate::document::detect_document_corners(&editor.original);
        }
        if close || context.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.editor = None;
        }
        if apply {
            self.apply_editor();
        }
    }

    fn review_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        if self.slots.is_empty() {
            self.reviewing = false;
            self.review_viewport.clear();
            return;
        }
        if self
            .selected_slot
            .is_none_or(|index| index >= self.slots.len())
        {
            self.selected_slot = Some(0);
        }

        let filename_id = Id::new("review-filename");
        let filename_has_focus = context.memory(|memory| memory.has_focus(filename_id));
        let keyboard_enabled = !self.has_blocking_dialog();
        let (arrow_left, arrow_right, escape) = if keyboard_enabled {
            context.input(|input| {
                (
                    input.key_pressed(egui::Key::ArrowLeft),
                    input.key_pressed(egui::Key::ArrowRight),
                    input.key_pressed(egui::Key::Escape),
                )
            })
        } else {
            (false, false, false)
        };
        if escape {
            self.reviewing = false;
            self.review_viewport.clear();
            return;
        }
        if !filename_has_focus {
            if arrow_left
                && let Some(index) = self.selected_slot
                && index > 0
            {
                self.selected_slot = Some(index - 1);
            }
            if arrow_right
                && let Some(index) = self.selected_slot
                && index + 1 < self.slots.len()
            {
                self.selected_slot = Some(index + 1);
            }
        }

        let index = self.selected_slot.unwrap_or(0);
        match self.slots.get(index) {
            Some(SlotEntry {
                id,
                revision,
                slot: PageSlot::Ready(data),
            }) => self.review_viewport.ensure_page(
                context,
                PageTextureKey {
                    id: *id,
                    revision: *revision,
                },
                &data.page.jpeg,
            ),
            _ => self.review_viewport.invalidate(),
        }

        let mut back = false;
        let mut save_now = false;
        let mut go_prev = false;
        let mut go_next = false;
        let mut move_left = false;
        let mut move_right = false;
        let mut rotate = false;
        let mut fix_crop = false;
        let mut delete_page = false;

        page_container(ui, |ui| {
            let page_count_text = polish_page_count(self.slots.len());
            two_sided(
                ui,
                48.0,
                |ui| {
                    ui.horizontal(|ui| {
                        back = ui.button("Wróć do skanowania (Esc)").clicked();
                        ui.add_space(10.0);
                        ui.heading(format!("Przegląd · {page_count_text}"));
                    });
                },
                |ui| {
                    ui.label(
                        RichText::new(format!("Strona {} z {}", index + 1, self.slots.len()))
                            .strong(),
                    );
                },
            );
            ui.add_space(8.0);

            let strip_height = 124.0;
            let body_size = Vec2::new(
                ui.available_width(),
                (ui.available_height() - strip_height - 10.0).max(280.0),
            );
            let (body_rect, _) = ui.allocate_exact_size(body_size, Sense::hover());
            let inspector_width = 282.0_f32.min((body_rect.width() * 0.36).max(240.0));
            let viewport_rect = Rect::from_min_max(
                body_rect.min,
                Pos2::new(
                    body_rect.right() - inspector_width - 12.0,
                    body_rect.bottom(),
                ),
            );
            let inspector_rect = Rect::from_min_max(
                Pos2::new(viewport_rect.right() + 12.0, body_rect.top()),
                body_rect.max,
            );

            let mut viewport_ui = ui.new_child(
                UiBuilder::new()
                    .id_salt("review-viewport")
                    .max_rect(viewport_rect)
                    .layout(Layout::top_down(Align::Min)),
            );
            match self.slots.get(index).map(|entry| &entry.slot) {
                Some(PageSlot::Ready(_)) => {
                    self.review_viewport.show(&mut viewport_ui);
                }
                Some(PageSlot::Processing) => review_status_card(
                    &mut viewport_ui,
                    "Przetwarzanie strony…",
                    "Zapis będzie dostępny po zakończeniu.",
                    false,
                ),
                Some(PageSlot::Reprocessing { .. }) => review_status_card(
                    &mut viewport_ui,
                    "Aktualizowanie kadru…",
                    "Pełny podgląd odświeży się automatycznie.",
                    false,
                ),
                Some(PageSlot::Failed { error, .. }) => review_status_card(
                    &mut viewport_ui,
                    "Nie udało się przygotować strony",
                    error,
                    true,
                ),
                None => {}
            }

            let mut inspector_ui = ui.new_child(
                UiBuilder::new()
                    .id_salt("review-inspector")
                    .max_rect(inspector_rect)
                    .layout(Layout::top_down(Align::Min)),
            );
            egui::ScrollArea::vertical()
                .id_salt("review-inspector-scroll")
                .auto_shrink([false, false])
                .show(&mut inspector_ui, |ui| {
                    ui.set_width((inspector_rect.width() - 12.0).max(1.0));
                    Frame::new()
                        .fill(Color32::WHITE)
                        .stroke(Stroke::new(1.0, Color32::from_gray(218)))
                        .corner_radius(10.0)
                        .inner_margin(16)
                        .show(ui, |ui| {
                            ui.set_min_height((inspector_rect.height() - 34.0).max(0.0));
                            ui.label(
                                RichText::new(format!("Strona {}", index + 1))
                                    .size(19.0)
                                    .strong(),
                            );
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui.add_enabled(index > 0, Button::new("◀")).clicked() {
                                    go_prev = true;
                                }
                                if ui
                                    .add_enabled(index + 1 < self.slots.len(), Button::new("▶"))
                                    .clicked()
                                {
                                    go_next = true;
                                }
                                ui.label(format!("{} / {}", index + 1, self.slots.len()));
                            });
                            ui.separator();

                            let rotatable = matches!(
                                self.slots.get(index).map(|entry| &entry.slot),
                                Some(PageSlot::Ready(_))
                            );
                            if ui
                                .add_enabled_ui(rotatable, |ui| {
                                    ui.add_sized([ui.available_width(), 38.0], Button::new("Obróć"))
                                })
                                .inner
                                .clicked()
                            {
                                rotate = true;
                            }
                            let editable = slot_has_editable_original(
                                self.slots.get(index).map(|entry| &entry.slot),
                            );
                            if ui
                                .add_enabled_ui(editable, |ui| {
                                    ui.add_sized(
                                        [ui.available_width(), 38.0],
                                        Button::new("Popraw kadr"),
                                    )
                                })
                                .inner
                                .clicked()
                            {
                                fix_crop = true;
                            }
                            ui.horizontal(|ui| {
                                if ui.add_enabled(index > 0, Button::new("← W lewo")).clicked() {
                                    move_left = true;
                                }
                                if ui
                                    .add_enabled(
                                        index + 1 < self.slots.len(),
                                        Button::new("W prawo →"),
                                    )
                                    .clicked()
                                {
                                    move_right = true;
                                }
                            });
                            if ui
                                .add_sized(
                                    [ui.available_width(), 38.0],
                                    Button::new(
                                        RichText::new("Usuń stronę").color(Color32::DARK_RED),
                                    ),
                                )
                                .clicked()
                            {
                                delete_page = true;
                            }

                            ui.separator();
                            ui.label(RichText::new("Nazwa pliku").strong());
                            let filename_response = ui.add_sized(
                                [ui.available_width(), 36.0],
                                egui::TextEdit::singleline(&mut self.filename)
                                    .id(filename_id)
                                    .hint_text("np. Umowa - Kowalski"),
                            );
                            if self.save_dialog_needs_focus {
                                self.save_dialog_needs_focus = false;
                                filename_response.request_focus();
                            }
                            if filename_response.changed() && self.editing_target.is_some() {
                                self.edit_dirty = true;
                            }

                            let target = self.resolve_save_target();
                            match &target {
                                Ok(plan) => {
                                    ui.label(
                                        RichText::new(format!("Zapis: {}", plan.path.display()))
                                            .small()
                                            .color(Color32::from_gray(90)),
                                    );
                                    if plan.mode == SaveMode::Copy {
                                        ui.label(
                                            RichText::new("Oryginał pozostanie bez zmian.")
                                                .small()
                                                .color(Color32::from_gray(90)),
                                        );
                                    }
                                }
                                Err(error) => {
                                    ui.label(RichText::new(error).small().color(Color32::DARK_RED));
                                }
                            }
                            ui.add_space(8.0);
                            let save_enabled = self.can_submit_save();
                            let save_label = if self.pending_jobs > 0 {
                                format!("Przetwarzanie {}…", self.pending_jobs)
                            } else {
                                "Zapisz PDF (Enter)".to_owned()
                            };
                            let save_clicked = ui
                                .add_enabled(
                                    save_enabled,
                                    Button::new(
                                        RichText::new(save_label).strong().color(Color32::WHITE),
                                    )
                                    .fill(BLUE)
                                    .corner_radius(9.0)
                                    .min_size(Vec2::new(ui.available_width(), 44.0)),
                                )
                                .clicked();
                            let enter_pressed = !self.has_blocking_dialog()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter));
                            let enter_allowed = filename_response.has_focus()
                                || context.memory(|memory| memory.focused().is_none());
                            save_now =
                                save_enabled && (save_clicked || (enter_pressed && enter_allowed));
                        });
                });

            ui.add_space(10.0);
            let strip_size = Vec2::new(ui.available_width(), strip_height);
            let (strip_rect, _) = ui.allocate_exact_size(strip_size, Sense::hover());
            let mut strip_ui = ui.new_child(
                UiBuilder::new()
                    .id_salt("review-strip")
                    .max_rect(strip_rect)
                    .layout(Layout::left_to_right(Align::Center)),
            );
            self.film_strip_ui(&mut strip_ui);
        });

        if back {
            self.reviewing = false;
            self.review_viewport.clear();
            return;
        }
        if go_prev && index > 0 {
            self.selected_slot = Some(index - 1);
        }
        if go_next && index + 1 < self.slots.len() {
            self.selected_slot = Some(index + 1);
        }
        if move_left && index > 0 {
            self.move_selected_page(-1);
        }
        if move_right && index + 1 < self.slots.len() {
            self.move_selected_page(1);
        }
        if rotate {
            self.rotate_selected_page(context);
        }
        if fix_crop {
            self.open_editor(index, context);
        }
        if delete_page {
            self.request_delete_selected_page();
        }
        if save_now {
            self.save_current_document();
        }
    }

    fn dialogs(&mut self, context: &egui::Context) {
        if self.show_restore {
            egui::Modal::new(Id::new("restore-session-modal")).show(context, |ui| {
                    ui.heading("Niezapisana sesja");
                    ui.add_space(8.0);
                    let (count, skipped, folder_display, folder_exists) = match &self.recovered {
                        Some(recovered) => (
                            recovered.pages.len() + recovered.skipped_pages,
                            recovered.skipped_pages,
                            recovered.folder_path.display().to_string(),
                            recovered.folder_path.is_dir(),
                        ),
                        None => (0, 0, String::new(), false),
                    };
                    ui.label(format!(
                        "Znaleziono niezapisaną sesję ({}).",
                        polish_page_count(count)
                    ));
                    ui.label(format!("Folder: {folder_display}"));
                    if skipped > 0 {
                        ui.label(
                            RichText::new(format!(
                                "{skipped} stron nie ma już obrazu ani oryginału. Pozostałe można odzyskać."
                            ))
                            .color(Color32::DARK_RED),
                        );
                    }
                    if !folder_exists {
                        ui.label(
                            RichText::new("Ten folder już nie istnieje — przywracanie niemożliwe.")
                                .color(Color32::DARK_RED),
                        );
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button(RichText::new("Usuń sesję").color(Color32::DARK_RED))
                            .clicked()
                        {
                            if let Some(session) = &self.session {
                                let _ = session.clear();
                            }
                            self.recovered = None;
                            self.show_restore = false;
                        }
                        if ui
                            .add_enabled(folder_exists, Button::new("Przywróć"))
                            .clicked()
                        {
                            self.show_restore = false;
                            self.restore_recovered_session(context);
                        }
                    });
                });
        }

        if self.show_new_folder {
            egui::Modal::new(Id::new("new-folder-modal")).show(context, |ui| {
                ui.heading("Nowy folder");
                ui.add_space(8.0);
                ui.label("Nazwa folderu:");
                let response = ui.add_sized(
                    [360.0, 34.0],
                    egui::TextEdit::singleline(&mut self.new_folder_name),
                );
                if context.memory(|memory| memory.focused().is_none()) {
                    response.request_focus();
                }
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
            egui::Modal::new(Id::new("rename-folder-modal")).show(context, |ui| {
                ui.heading("Zmień nazwę folderu");
                ui.add_space(8.0);
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
            egui::Modal::new(Id::new("settings-modal")).show(context, |ui| {
                ui.heading("Ustawienia");
                ui.add_space(8.0);
                ui.label(RichText::new("Folder biblioteki").strong());
                ui.label(self.library_root.display().to_string());
                ui.add_space(10.0);
                if ui.button("Zmień lokalizację").clicked() {
                    if self.has_active_workflow() {
                        self.show_settings = false;
                        self.message =
                            Some("Najpierw zapisz albo anuluj bieżący dokument.".to_owned());
                    } else if let Some(path) = rfd::FileDialog::new().pick_folder() {
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
                }
                ui.add_space(12.0);
                if primary_button(ui, "Gotowe").clicked() {
                    self.show_settings = false;
                }
            });
        }

        if let Some(pdf) = self.delete_pdf_target.clone() {
            egui::Modal::new(Id::new("delete-document-modal")).show(context, |ui| {
                ui.heading("Usunąć dokument?");
                ui.add_space(8.0);
                ui.set_max_width(480.0);
                ui.label(format!("Plik „{}” zostanie trwale usunięty.", pdf.name));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Anuluj").clicked() {
                        self.delete_pdf_target = None;
                    }
                    if ui
                        .button(RichText::new("Usuń dokument").color(Color32::DARK_RED))
                        .clicked()
                    {
                        self.delete_pdf_target = None;
                        match std::fs::remove_file(&pdf.path) {
                            Ok(()) => {
                                self.refresh_pdfs();
                                self.refresh_folders();
                            }
                            Err(error) => {
                                self.message = Some(format!("Nie można usunąć pliku: {error}"));
                            }
                        }
                    }
                });
            });
        }

        if self.show_cancel_confirm {
            let editing_pdf = self.editing_target.is_some();
            let title = if editing_pdf {
                "Anulować edycję?"
            } else {
                "Anulować dokument?"
            };
            egui::Modal::new(Id::new("cancel-document-modal")).show(context, |ui| {
                ui.heading(title);
                ui.add_space(8.0);
                if editing_pdf {
                    ui.label("Niezapisane zmiany w dokumencie zostaną utracone.");
                } else {
                    ui.label(format!(
                        "Zeskanowane strony ({}) zostaną utracone.",
                        self.slots.len()
                    ));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let return_label = if editing_pdf {
                        "Wróć do edycji"
                    } else {
                        "Wróć do skanowania"
                    };
                    if ui.button(return_label).clicked() {
                        self.show_cancel_confirm = false;
                    }
                    let cancel_label = if editing_pdf {
                        "Anuluj edycję"
                    } else {
                        "Anuluj dokument"
                    };
                    if ui
                        .button(RichText::new(cancel_label).color(Color32::DARK_RED))
                        .clicked()
                    {
                        self.show_cancel_confirm = false;
                        self.abandon_scan();
                    }
                });
            });
        }

        if let Some(target_id) = self.delete_page_target {
            egui::Modal::new(Id::new("delete-page-modal")).show(context, |ui| {
                ui.heading("Usunąć stronę?");
                ui.add_space(8.0);
                let page_number = self
                    .slots
                    .iter()
                    .position(|entry| entry.id == target_id)
                    .map(|index| index + 1);
                ui.label(match page_number {
                    Some(number) => format!("Usunąć stronę {number}?"),
                    None => "Ta strona już nie istnieje.".to_owned(),
                });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Anuluj").clicked() {
                        self.delete_page_target = None;
                    }
                    if ui
                        .add_enabled(
                            page_number.is_some(),
                            Button::new(RichText::new("Usuń stronę").color(Color32::DARK_RED)),
                        )
                        .clicked()
                    {
                        self.delete_page_target = None;
                        self.delete_page_by_id(target_id);
                    }
                });
            });
        }

        if self.show_exit_confirm {
            egui::Modal::new(Id::new("exit-without-saving-modal")).show(context, |ui| {
                ui.heading("Niezapisany dokument");
                ui.add_space(8.0);
                ui.label("Zeskanowane strony nie zostały jeszcze zapisane.");
                ui.label("Czy na pewno chcesz zamknąć program?");
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Wróć do dokumentu").clicked() {
                        self.show_exit_confirm = false;
                    }
                    if ui
                        .button(RichText::new("Zamknij bez zapisywania").color(Color32::DARK_RED))
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
            egui::Modal::new(Id::new("information-modal")).show(context, |ui| {
                ui.heading("Informacja");
                ui.add_space(8.0);
                ui.set_max_width(480.0);
                ui.label(message);
                ui.add_space(10.0);
                if primary_button(ui, "OK").clicked() {
                    self.message = None;
                }
            });
        }

        let toast_lifetime = if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.pdf_path.is_some())
        {
            Duration::from_secs(8)
        } else {
            Duration::from_secs(4)
        };
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.shown_at.elapsed() > toast_lifetime)
        {
            self.toast = None;
        }
        if let Some(toast) = &self.toast {
            let toast_lift = if self.screen == Screen::ScanHub && self.editor.is_none() {
                -(STRIP_HEIGHT + 44.0)
            } else {
                -24.0
            };
            let mut open_error = None;
            egui::Area::new(Id::new("zapis-toast"))
                .anchor(egui::Align2::RIGHT_BOTTOM, Vec2::new(-24.0, toast_lift))
                .show(context, |ui| {
                    Frame::new()
                        .fill(Color32::from_rgb(34, 120, 62))
                        .corner_radius(10.0)
                        .inner_margin(Margin::symmetric(16, 10))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(&toast.text).color(Color32::WHITE).size(16.0),
                                );
                                if let Some(path) = &toast.pdf_path
                                    && ui
                                        .add(
                                            Button::new(
                                                RichText::new("Otwórz PDF")
                                                    .strong()
                                                    .color(Color32::from_rgb(34, 120, 62)),
                                            )
                                            .fill(Color32::WHITE)
                                            .corner_radius(8.0),
                                        )
                                        .clicked()
                                    && let Err(error) = open::that_detached(path)
                                {
                                    open_error = Some(format!("Nie można otworzyć PDF: {error}"));
                                }
                            });
                        });
                });
            if let Some(error) = open_error {
                self.message = Some(error);
            }
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
        let dialog_open = self.reviewing || self.editor.is_some() || self.has_blocking_dialog();
        let focus_free = context.memory(|memory| memory.focused().is_none());
        if self.screen == Screen::ScanHub && !dialog_open && focus_free {
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
                self.reviewing = true;
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
    let width = (ui.max_rect().right() - min.x).max(0.0);
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

fn review_status_card(ui: &mut egui::Ui, title: &str, detail: &str, failed: bool) {
    let border = if failed {
        Color32::from_rgb(205, 92, 92)
    } else {
        Color32::from_gray(205)
    };
    Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(10.0)
        .inner_margin(28)
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.with_layout(
                Layout::centered_and_justified(egui::Direction::TopDown),
                |ui| {
                    ui.vertical_centered(|ui| {
                        if !failed {
                            ui.spinner();
                            ui.add_space(10.0);
                        }
                        ui.label(RichText::new(title).size(20.0).strong());
                        ui.label(RichText::new(detail).color(if failed {
                            Color32::DARK_RED
                        } else {
                            Color32::GRAY
                        }));
                    });
                },
            );
        });
}

fn rollback_reprocessing(
    previous: Option<Box<PageData>>,
    original_jpeg: Vec<u8>,
    quarter_turns: u8,
    error: String,
) -> PageSlot {
    if let Some(mut previous) = previous {
        previous.original_jpeg = original_jpeg;
        PageSlot::Ready(previous)
    } else {
        PageSlot::Failed {
            original_jpeg,
            error,
            quarter_turns,
        }
    }
}

fn reprocess_failure_message(had_previous: bool, error: &str) -> String {
    if had_previous {
        format!("Nie udało się zastosować kadru. Zachowano poprzednią wersję strony: {error}")
    } else {
        format!("Nie udało się zastosować kadru. Strona nadal wymaga poprawy: {error}")
    }
}

fn slot_has_editable_original(slot: Option<&PageSlot>) -> bool {
    match slot {
        Some(PageSlot::Ready(data)) => !data.original_jpeg.is_empty(),
        Some(PageSlot::Failed { original_jpeg, .. }) => !original_jpeg.is_empty(),
        _ => false,
    }
}

fn file_fingerprint(path: &std::path::Path) -> Result<u64, String> {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let bytes = std::fs::read(path)
        .map_err(|error| format!("Nie można sprawdzić pliku źródłowego: {error}"))?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Ok(hasher.finish())
}

fn pdf_io_error(error: std::io::Error) -> String {
    format!("Nie można ukończyć zapisu pliku PDF: {error}")
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

fn draw_progress_outline(painter: &egui::Painter, points: &[Pos2; 4], progress: f32) {
    let mut lengths = [0.0_f32; 4];
    let mut total = 0.0;
    for edge in 0..4 {
        let start = points[edge];
        let end = points[(edge + 1) % 4];
        lengths[edge] = ((end.x - start.x).powi(2) + (end.y - start.y).powi(2)).sqrt();
        total += lengths[edge];
    }
    if total <= 0.0 {
        return;
    }
    let mut remaining = total * progress.clamp(0.0, 1.0);
    for edge in 0..4 {
        if remaining <= 0.0 {
            break;
        }
        let start = points[edge];
        let end = points[(edge + 1) % 4];
        let take = lengths[edge].min(remaining);
        let fraction = if lengths[edge] > 0.0 {
            take / lengths[edge]
        } else {
            0.0
        };
        let tip = Pos2::new(
            start.x + (end.x - start.x) * fraction,
            start.y + (end.y - start.y) * fraction,
        );
        painter.line_segment(
            [start, tip],
            Stroke::new(6.0, Color32::from_rgb(255, 200, 60)),
        );
        remaining -= take;
    }
}

fn fallback_editor_corners() -> [CropPoint; 4] {
    [
        CropPoint::new(0.06, 0.06),
        CropPoint::new(0.94, 0.06),
        CropPoint::new(0.94, 0.94),
        CropPoint::new(0.06, 0.94),
    ]
}

fn full_frame_editor_corners() -> [CropPoint; 4] {
    [
        CropPoint::new(0.0, 0.0),
        CropPoint::new(1.0, 0.0),
        CropPoint::new(1.0, 1.0),
        CropPoint::new(0.0, 1.0),
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

fn can_leave_scan_without_confirmation(
    slots_empty: bool,
    editing_pdf: bool,
    edit_dirty: bool,
) -> bool {
    slots_empty || (editing_pdf && !edit_dirty)
}

fn has_active_workflow_state(
    screen: Screen,
    pipeline_active: bool,
    has_pages: bool,
    has_pending_jobs: bool,
    editing_pdf: bool,
) -> bool {
    screen == Screen::ScanHub || pipeline_active || has_pages || has_pending_jobs || editing_pdf
}

#[cfg(test)]
mod navigation_tests {
    use super::{Screen, can_leave_scan_without_confirmation, has_active_workflow_state};

    #[test]
    fn empty_scan_can_return_to_folder() {
        assert!(can_leave_scan_without_confirmation(true, false, false));
    }

    #[test]
    fn unchanged_pdf_edit_can_return_to_folder() {
        assert!(can_leave_scan_without_confirmation(false, true, false));
    }

    #[test]
    fn modified_pdf_edit_requires_confirmation() {
        assert!(!can_leave_scan_without_confirmation(false, true, true));
    }

    #[test]
    fn unsaved_scan_requires_confirmation() {
        assert!(!can_leave_scan_without_confirmation(false, false, false));
    }

    #[test]
    fn scan_hub_blocks_library_changes_even_before_first_capture() {
        assert!(has_active_workflow_state(
            Screen::ScanHub,
            false,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn idle_library_allows_library_changes() {
        assert!(!has_active_workflow_state(
            Screen::Library,
            false,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn background_work_blocks_library_changes() {
        assert!(has_active_workflow_state(
            Screen::Folder,
            false,
            false,
            true,
            false,
        ));
    }
}
