use crate::autocapture::{AutoCapture, FeedResult};
use crate::camera::{CameraController, CameraEvent};
use crate::document::{
    CropPoint, ScannedPage, extract_pdf_pages, page_from_jpeg_bytes, pages_from_extracted,
    render_pdf, rotate_rgb,
};
use crate::library::{
    FileActionReport, FileOperationEvent, FileOperationWorker, LibraryEvent, LibrarySnapshot,
    LibraryWorker, normalize_search_text, rename_pdf,
};
use crate::library_view::{LibraryAction, LibraryViewInput, LibraryViewState, show_library_view};
use crate::overlay::OverlayDetector;
use crate::pipeline::{PipelineEvent, ProcessingPipeline};
use crate::review_viewport::{PageTextureKey, ReviewViewport};
use crate::session::{RecoveredSession, SessionStore, SessionWorker};
use crate::storage::{
    FolderInfo, PdfInfo, Settings, create_folder, default_library_root, ensure_library,
    load_settings, next_sequence_name, normalized_pdf_stem, pdf_info, rename_folder, save_settings,
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
const CONTROL_HEIGHT: f32 = 44.0;

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

#[derive(Clone, Copy)]
enum PendingFileOperation {
    Move,
    Recycle,
}

#[derive(PartialEq, Eq)]
struct SaveTargetKey {
    filename: String,
    folder: Option<PathBuf>,
    editing_target: Option<PathBuf>,
    library_revision: u64,
}

#[derive(Clone)]
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
    library_worker: LibraryWorker,
    file_operation_worker: FileOperationWorker,
    pending_file_operation: Option<PendingFileOperation>,
    library_snapshot: LibrarySnapshot,
    library_revision: u64,
    library_loading: bool,
    library_restore_pending: bool,
    last_library_refresh: Instant,
    library_view: LibraryViewState,

    camera: Option<CameraController>,
    camera_status: String,
    camera_ready: bool,
    camera_failed: bool,
    save_target_cache: Option<(SaveTargetKey, Result<SavePlan, String>)>,
    last_overlay_submit: Option<Instant>,
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
    session: Option<SessionWorker>,
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
    rename_pdf_target: Option<PathBuf>,
    rename_pdf_name: String,
    move_pdf_targets: Vec<PathBuf>,
    move_pdf_destination: Option<PathBuf>,
    move_folder_query: String,
    delete_pdf_targets: Vec<PathBuf>,
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
        let mut library_worker = LibraryWorker::start();
        let library_loading = library_worker.request_refresh(library_root.clone()).is_ok();
        let mut app = Self {
            screen: Screen::Library,
            settings,
            library_root: library_root.clone(),
            folders: Vec::new(),
            selected_folder: None,
            pdfs: Vec::new(),
            library_worker,
            file_operation_worker: FileOperationWorker::start(),
            pending_file_operation: None,
            library_snapshot: LibrarySnapshot {
                root: library_root.clone(),
                ..LibrarySnapshot::default()
            },
            library_revision: 0,
            library_loading,
            library_restore_pending: true,
            last_library_refresh: Instant::now(),
            library_view: LibraryViewState::default(),
            camera: None,
            camera_status: String::new(),
            camera_ready: false,
            camera_failed: false,
            save_target_cache: None,
            last_overlay_submit: None,
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
            rename_pdf_target: None,
            rename_pdf_name: String::new(),
            move_pdf_targets: Vec::new(),
            move_pdf_destination: None,
            move_folder_query: String::new(),
            delete_pdf_targets: Vec::new(),
            show_exit_confirm: false,
            allow_exit: false,
            message: None,
        };
        if let Err(error) = ensure_library(&app.library_root) {
            app.message = Some(error);
        }
        {
            // Housekeeping for crash leftovers of the atomic save machinery;
            // results show up through the normal library refresh.
            let root = app.library_root.clone();
            std::thread::spawn(move || {
                let _ = crate::library::sweep_recovery_artifacts(
                    &root,
                    Duration::from_secs(24 * 60 * 60),
                );
            });
        }
        if let Some(store) = SessionStore::open_default() {
            if let Some(recovered) = store.load_existing() {
                app.recovered = Some(recovered);
                app.show_restore = true;
            }
            app.session = Some(SessionWorker::start(store));
        }
        app
    }

    fn restore_last_folder(&mut self) {
        let Some(folder) = self
            .folders
            .iter()
            .find(|folder| {
                self.settings
                    .last_folder_path
                    .as_ref()
                    .is_some_and(|path| path == &folder.path)
                    || self
                        .settings
                        .last_folder
                        .as_ref()
                        .is_some_and(|name| name == &folder.name)
            })
            .cloned()
        else {
            return;
        };
        self.open_folder(folder);
    }

    fn refresh_folders(&mut self) {
        match self
            .library_worker
            .request_refresh(self.library_root.clone())
        {
            Ok(_) => {
                self.library_loading = true;
                self.last_library_refresh = Instant::now();
            }
            Err(error) => {
                self.library_loading = false;
                self.message = Some(error);
            }
        }
    }

    fn refresh_pdfs(&mut self) {
        let Some(folder) = &self.selected_folder else {
            self.pdfs.clear();
            return;
        };
        self.pdfs = self
            .library_snapshot
            .pdfs
            .iter()
            .filter(|pdf| pdf.folder_path == folder.path)
            .cloned()
            .collect();
    }

    fn poll_library(&mut self, context: &egui::Context) {
        if let Some(event) = self.library_worker.try_recv_latest() {
            self.library_loading = false;
            self.last_library_refresh = Instant::now();
            match event {
                LibraryEvent::Refreshed { snapshot, .. } => {
                    if snapshot.root != self.library_root {
                        self.refresh_folders();
                        return;
                    }
                    self.library_snapshot = snapshot;
                    self.library_revision = self.library_revision.saturating_add(1);
                    self.folders = self.library_snapshot.folders.clone();
                    if let Some(selected_path) = self
                        .selected_folder
                        .as_ref()
                        .map(|folder| folder.path.clone())
                    {
                        self.selected_folder = if selected_path == self.library_root {
                            Some(FolderInfo {
                                name: "Główna lokalizacja".to_owned(),
                                path: self.library_root.clone(),
                                pdf_count: self
                                    .library_snapshot
                                    .pdfs
                                    .iter()
                                    .filter(|pdf| pdf.folder_path == self.library_root)
                                    .count(),
                                modified_secs: 0,
                            })
                        } else {
                            self.folders
                                .iter()
                                .find(|folder| folder.path == selected_path)
                                .cloned()
                        };
                    }
                    self.refresh_pdfs();
                    if self.library_restore_pending {
                        self.library_restore_pending = false;
                        if self.selected_folder.is_none() {
                            self.restore_last_folder();
                        }
                    }
                    let recent_count = self.settings.recent_documents.len();
                    self.settings.recent_documents.retain(|path| {
                        self.library_snapshot
                            .pdfs
                            .iter()
                            .any(|pdf| &pdf.path == path)
                    });
                    if self.settings.recent_documents.len() != recent_count {
                        let _ = save_settings(&self.settings);
                    }
                }
                LibraryEvent::Failed { error, .. } => self.message = Some(error),
            }
        }

        if self.screen != Screen::ScanHub
            && !self.library_loading
            && self.last_library_refresh.elapsed() >= Duration::from_secs(30)
        {
            self.refresh_folders();
        }
        if self.library_loading {
            context.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn open_folder(&mut self, folder: FolderInfo) {
        let remember_as_scan_destination = folder.path != self.library_root;
        if remember_as_scan_destination {
            self.settings.last_folder = Some(folder.name.clone());
            self.settings.last_folder_path = Some(folder.path.clone());
        }
        self.selected_folder = Some(folder);
        self.library_view.section = crate::library_view::LibrarySection::All;
        self.library_view.query.clear();
        self.screen = Screen::Folder;
        self.refresh_pdfs();
        if remember_as_scan_destination {
            let _ = save_settings(&self.settings);
        }
    }

    fn remember_recent_document(&mut self, path: &std::path::Path) {
        self.settings
            .recent_documents
            .retain(|recent| recent != path);
        self.settings.recent_documents.insert(0, path.to_path_buf());
        self.settings.recent_documents.truncate(50);
        let _ = save_settings(&self.settings);
    }

    fn replace_recent_document_path(&mut self, source: &std::path::Path, target: &std::path::Path) {
        let mut changed = false;
        for recent in &mut self.settings.recent_documents {
            if recent == source {
                *recent = target.to_path_buf();
                changed = true;
            }
        }
        if changed {
            let _ = save_settings(&self.settings);
        }
    }

    fn begin_scan(&mut self) {
        self.slots.clear();
        self.selected_slot = None;
        self.pending_jobs = 0;
        self.filename = self
            .selected_folder
            .as_ref()
            .map(|folder| next_sequence_name(&self.pdfs, &folder.name))
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
        if let (Some(session), Some(folder)) = (&self.session, &self.selected_folder) {
            session.begin(&folder.path);
        }
        self.start_camera();
    }

    /// Persistence errors surface asynchronously; the first one disables the
    /// session copy for the rest of the document, exactly as before.
    fn poll_session_errors(&mut self) {
        if self.session_broken {
            return;
        }
        if let Some(error) = self.session.as_ref().and_then(SessionWorker::try_recv_error) {
            self.session_broken = true;
            self.toast = Some(Toast {
                text: format!("Kopia sesji wyłączona: {error}"),
                shown_at: Instant::now(),
                pdf_path: None,
            });
        }
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
        if let Some(session) = &self.session {
            session.write_page(id, jpeg, original_jpeg, corners, quarter_turns);
        }
    }

    fn session_sync_order(&mut self) {
        if self.session_broken {
            return;
        }
        let ids: Vec<u64> = self.slots.iter().map(|entry| entry.id).collect();
        if let Some(session) = &self.session {
            session.set_order(ids);
        }
    }

    fn session_clear(&mut self) {
        if let Some(session) = &self.session {
            session.clear();
        }
    }

    fn start_camera(&mut self) {
        self.stop_camera();
        self.camera_status = "Łączenie z IRIScan Visualizer 7…".to_owned();
        self.camera_ready = false;
        self.camera_failed = false;
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
                        // The detector samples at ~3 Hz; cloning the ~3.7 MB
                        // preview for every camera frame would be wasted work.
                        if let Some(overlay) = &self.overlay
                            && self
                                .last_overlay_submit
                                .is_none_or(|at| at.elapsed() >= Duration::from_millis(300))
                        {
                            overlay.submit(image.clone());
                            self.last_overlay_submit = Some(Instant::now());
                        }
                        self.pending_preview = Some(image);
                    }
                }
                CameraEvent::Error(error) => {
                    self.camera_ready = false;
                    self.camera_failed = true;
                    self.preview_texture = None;
                    self.camera_status = error;
                    if let Some(camera) = &self.camera {
                        camera.clear_latest_frame();
                    }
                }
            }
        }
        if self.screen == Screen::ScanHub && live_preview && !self.camera_failed {
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
        if !self.camera_ready {
            if manual {
                self.message = Some("Poczekaj, aż pojawi się obraz z kamery.".to_owned());
            }
            return false;
        }
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
        let mut pipeline_dead = false;
        if let Some(pipeline) = &self.pipeline {
            // Read the death flag before draining: an event arriving between
            // the drain and the check must not be mistaken for a lost job.
            pipeline_dead = pipeline.is_dead();
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
                    let texture = strip_texture(context, id, 0, &page, 0);
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
                    if let Some(entry) = self.slots.iter_mut().find(|entry| entry.id == id) {
                        let current = std::mem::replace(&mut entry.slot, PageSlot::Processing);
                        let (original_jpeg, quarter_turns) = match current {
                            PageSlot::Reprocessing {
                                original_jpeg,
                                quarter_turns,
                                previous: _,
                            } => (original_jpeg, quarter_turns),
                            other => {
                                entry.slot = other;
                                continue;
                            }
                        };
                        // Rotation stays metadata: the reprocessed JPEG is kept
                        // unrotated and only the strip texture is rotated.
                        let texture =
                            strip_texture(context, entry.id, entry.revision + 1, &page, quarter_turns);
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
        if pipeline_dead {
            self.recover_dead_pipeline();
        }
        if self.pending_jobs > 0 {
            context.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// The worker thread panicked: its in-flight and queued jobs are gone.
    /// Turn the affected slots into visible failures and start a fresh worker
    /// so scanning can continue — the crash-recovery copy stays untouched.
    fn recover_dead_pipeline(&mut self) {
        self.pipeline = Some(ProcessingPipeline::start());
        self.pending_jobs = 0;
        let mut lost_pages = 0;
        for entry in &mut self.slots {
            match std::mem::replace(&mut entry.slot, PageSlot::Processing) {
                PageSlot::Processing => {
                    lost_pages += 1;
                    entry.slot = PageSlot::Failed {
                        original_jpeg: Vec::new(),
                        error: "Przetwarzanie tej strony uległo awarii. Usuń ją i zeskanuj ponownie."
                            .to_owned(),
                        quarter_turns: 0,
                    };
                }
                PageSlot::Reprocessing {
                    original_jpeg,
                    quarter_turns,
                    previous,
                } => {
                    lost_pages += 1;
                    entry.slot = rollback_reprocessing(
                        previous,
                        original_jpeg,
                        quarter_turns,
                        "Przetwarzanie kadru uległo awarii.".to_owned(),
                    );
                }
                other => entry.slot = other,
            }
        }
        self.message = Some(if lost_pages > 0 {
            format!(
                "Moduł przetwarzania stron uległ awarii i został uruchomiony ponownie. Dotknięte strony ({lost_pages}) oznaczono symbolem ⚠. Wcześniej zeskanowane strony są bezpieczne."
            )
        } else {
            "Moduł przetwarzania stron uległ awarii i został uruchomiony ponownie.".to_owned()
        });
    }

    fn can_save(&self) -> bool {
        !self.slots.is_empty()
            && self.pending_jobs == 0
            && self
                .slots
                .iter()
                .all(|entry| matches!(entry.slot, PageSlot::Ready(_)))
    }

    /// `resolve_save_target` probes the disk (`unique_pdf_path`), so the
    /// review screen must not call it every frame — the result is cached
    /// until the filename, folder, edit target or library snapshot changes.
    fn cached_save_target(&mut self) -> Result<SavePlan, String> {
        let key = SaveTargetKey {
            filename: self.filename.clone(),
            folder: self
                .selected_folder
                .as_ref()
                .map(|folder| folder.path.clone()),
            editing_target: self.editing_target.clone(),
            library_revision: self.library_revision,
        };
        if let Some((cached_key, result)) = &self.save_target_cache
            && cached_key == &key
        {
            return result.clone();
        }
        let result = self.resolve_save_target();
        self.save_target_cache = Some((key, result.clone()));
        result
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
            || self.rename_pdf_target.is_some()
            || !self.move_pdf_targets.is_empty()
            || !self.delete_pdf_targets.is_empty()
            || self.pending_file_operation.is_some()
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

    /// Instant and lossless: bumps the metadata quarter turn and rebuilds only
    /// the small strip texture — the page JPEG is never re-encoded.
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
        entry.revision += 1;
        data.quarter_turns = (data.quarter_turns + 1) % 4;
        data.texture = strip_texture(context, entry.id, entry.revision, &data.page, data.quarter_turns);
        if self.editing_target.is_some() {
            self.edit_dirty = true;
        }
        let written = (
            entry.id,
            data.page.jpeg.clone(),
            data.original_jpeg.clone(),
            data.corners,
            data.quarter_turns,
        );
        let (id, jpeg, original_jpeg, corners, quarter_turns) = written;
        self.session_write_page(id, &jpeg, &original_jpeg, corners, quarter_turns);
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
            session.remove_page(removed_id);
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
                PageSlot::Ready(data) => pages.push((&data.page, data.quarter_turns)),
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
                self.remember_recent_document(&path);
                self.filename.clear();
                self.abandon_scan();
                self.upsert_saved_pdf(&path);
                self.refresh_folders();
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn upsert_saved_pdf(&mut self, path: &std::path::Path) {
        self.update_snapshot_files(&[path.to_path_buf()], &[path.to_path_buf()]);
    }

    fn update_snapshot_files(&mut self, removed: &[PathBuf], added: &[PathBuf]) {
        if removed.is_empty() && added.is_empty() {
            return;
        }
        let removed: std::collections::HashSet<&std::path::Path> =
            removed.iter().map(PathBuf::as_path).collect();
        self.library_snapshot
            .pdfs
            .retain(|existing| !removed.contains(existing.path.as_path()));
        for path in added {
            if let Ok(pdf) = pdf_info(path) {
                self.library_snapshot
                    .pdfs
                    .retain(|existing| existing.path != pdf.path);
                self.library_snapshot.pdfs.push(pdf);
            }
        }
        for folder in &mut self.library_snapshot.folders {
            folder.pdf_count = self
                .library_snapshot
                .pdfs
                .iter()
                .filter(|item| item.folder_path == folder.path)
                .count();
        }
        self.library_revision = self.library_revision.saturating_add(1);
        self.refresh_pdfs();
    }

    fn update_snapshot_folder(&mut self, previous: &FolderInfo, renamed: &FolderInfo) {
        for folder in &mut self.library_snapshot.folders {
            if folder.path == previous.path {
                *folder = renamed.clone();
            }
        }
        for pdf in &mut self.library_snapshot.pdfs {
            if pdf.folder_path == previous.path {
                if let Ok(suffix) = pdf.path.strip_prefix(&previous.path) {
                    pdf.path = renamed.path.join(suffix);
                }
                pdf.folder_path = renamed.path.clone();
                pdf.folder_name = renamed.name.clone();
            }
        }
        self.folders = self.library_snapshot.folders.clone();
        self.library_revision = self.library_revision.saturating_add(1);
        self.refresh_pdfs();
    }

    fn upsert_snapshot_folder(&mut self, folder: &FolderInfo) {
        self.library_snapshot
            .folders
            .retain(|existing| existing.path != folder.path);
        self.library_snapshot.folders.push(folder.clone());
        self.library_snapshot
            .folders
            .sort_by_key(|item| item.name.to_lowercase());
        self.folders = self.library_snapshot.folders.clone();
        self.library_revision = self.library_revision.saturating_add(1);
    }

    fn commit_pdf_update(&self, path: PathBuf, bytes: &[u8]) -> Result<PathBuf, String> {
        let source = self
            .editing_target
            .as_ref()
            .ok_or_else(|| "Brak pliku źródłowego do aktualizacji.".to_owned())?;
        // Rendering and fsync can take noticeable time for a large PDF. Finish
        // that work before the short critical section below.
        let prepared = crate::atomic_file::prepare(&path, bytes).map_err(pdf_io_error)?;
        let _update_lock = crate::atomic_file::try_lock_for_update(source).map_err(|error| {
            format!(
                "Ten PDF jest właśnie zapisywany w innej sesji programu. Spróbuj ponownie za chwilę. Błąd: {error}"
            )
        })?;
        let current_fingerprint = file_fingerprint(source)?;
        let expected_fingerprint = self
            .editing_source_fingerprint
            .ok_or_else(|| "Brak odcisku pliku źródłowego.".to_owned())?;
        if expected_fingerprint != current_fingerprint {
            return Err(
                "Plik źródłowy zmienił się poza programem. Zapisz dokument pod inną nazwą, aby nie nadpisać cudzych zmian."
                    .to_owned(),
            );
        }
        prepared
            .commit_replace_if_unchanged(&path, expected_fingerprint)
            .map_err(pdf_io_error)?;
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
            modified_secs: 0,
        });
        // The session may have been redirected (original folder gone → library
        // root); keep the persisted manifest pointing at the actual target.
        if let Some(session) = &self.session {
            session.set_folder(&recovered.folder_path);
        }
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
        let mut max_id = recovered.highest_page_id;
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
                    let texture = strip_texture(context, id, 0, &page, quarter_turns);
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
                // The full-resolution original stays as the reprocess source;
                // only the displayed texture is reduced to the GPU limit.
                let max_texture_side = context.input(|input| input.max_texture_side);
                let scaled = crate::review_viewport::fit_texture_image(&original, max_texture_side);
                let texture = context.load_texture(
                    format!("edytor-{}", entry.id),
                    rgb_to_color_image(scaled.as_ref().unwrap_or(&original)),
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
        let pages = match pages_from_extracted(jpegs) {
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
        for (page, quarter_turns) in pages {
            let id = self.next_page_id;
            self.next_page_id += 1;
            // An imported PDF only contains the already processed page, not the
            // original camera frame. Treating it as a camera original would
            // apply perspective correction and JPEG compression a second time.
            let original_jpeg = Vec::new();
            let corners = full_frame_editor_corners();
            self.session_write_page(id, &page.jpeg, &original_jpeg, corners, quarter_turns);
            let texture = strip_texture(context, id, 0, &page, quarter_turns);
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
        self.filename = stem;
        self.editing_target = Some(target);
        self.editing_source_fingerprint = Some(source_fingerprint);
        self.edit_dirty = false;
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        Frame::new()
            .fill(Color32::WHITE)
            .inner_margin(Margin::symmetric(20, 8))
            .show(ui, |ui| {
                two_sided(
                    ui,
                    CONTROL_HEIGHT,
                    |ui| {
                        ui.label(
                            RichText::new("Skaner dokumentów")
                                .size(24.0)
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
                            let width = ui.available_width().min(280.0);
                            if width >= 100.0 {
                                ui.add_sized(
                                    [width, CONTROL_HEIGHT],
                                    egui::Label::new(
                                        RichText::new(&folder.name)
                                            .size(16.0)
                                            .color(Color32::DARK_GRAY),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&folder.name);
                            }
                        }
                    },
                );
            });
    }

    fn library_ui(&mut self, ui: &mut egui::Ui) {
        self.document_library_ui(ui);
    }

    fn folder_ui(&mut self, ui: &mut egui::Ui) {
        self.document_library_ui(ui);
    }

    fn document_library_ui(&mut self, ui: &mut egui::Ui) {
        let selected_folder = self
            .selected_folder
            .as_ref()
            .map(|folder| folder.path.as_path());
        let keyboard_enabled = !self.has_blocking_dialog();
        let actions = Frame::new()
            .inner_margin(Margin::symmetric(16, 10))
            .show(ui, |ui| {
                show_library_view(
                    ui,
                    &mut self.library_view,
                    LibraryViewInput {
                        snapshot: &self.library_snapshot,
                        snapshot_revision: self.library_revision,
                        recent_documents: &self.settings.recent_documents,
                        selected_folder,
                        preferred_scan_folder: self.settings.last_folder_path.as_deref(),
                        library_root: &self.library_root,
                        loading: self.library_loading,
                        keyboard_enabled,
                    },
                )
            })
            .inner;

        for action in actions {
            self.handle_library_action(action, ui.ctx());
        }
    }

    fn handle_library_action(&mut self, action: LibraryAction, context: &egui::Context) {
        match action {
            LibraryAction::ChangeSection(section) => {
                self.selected_folder = None;
                self.screen = Screen::Library;
                self.library_view.section = section;
                self.refresh_pdfs();
            }
            LibraryAction::SelectFolder(path) => {
                if let Some(folder) = self
                    .library_snapshot
                    .folders
                    .iter()
                    .find(|folder| folder.path == path)
                    .cloned()
                {
                    self.open_folder(folder);
                } else {
                    self.message = Some("Ten folder już nie istnieje.".to_owned());
                    self.refresh_folders();
                }
            }
            LibraryAction::RenameFolder(path) => {
                let Some(folder) = self
                    .library_snapshot
                    .folders
                    .iter()
                    .find(|folder| folder.path == path)
                    .cloned()
                else {
                    self.message = Some("Ten folder już nie istnieje.".to_owned());
                    self.refresh_folders();
                    return;
                };
                self.open_folder(folder.clone());
                self.rename_folder_name = folder.name;
                self.show_rename_folder = true;
            }
            LibraryAction::NewFolder => {
                self.new_folder_name.clear();
                self.show_new_folder = true;
            }
            LibraryAction::NewScan(destination) => {
                if let Some(path) = destination
                    && let Some(folder) = self
                        .library_snapshot
                        .folders
                        .iter()
                        .find(|folder| folder.path == path)
                        .cloned()
                {
                    self.open_folder(folder);
                }
                if self.selected_folder.is_some() {
                    self.begin_scan();
                } else {
                    self.message =
                        Some("Wybierz folder docelowy przed rozpoczęciem skanowania.".to_owned());
                }
            }
            LibraryAction::Refresh => self.refresh_folders(),
            LibraryAction::Open(path) => {
                if let Err(error) = open::that_detached(&path) {
                    self.message = Some(format!("Nie można otworzyć PDF: {error}"));
                } else {
                    self.remember_recent_document(&path);
                }
            }
            LibraryAction::Edit(path) => {
                let Some(pdf) = self.pdf_by_path(&path) else {
                    self.message = Some("Ten dokument już nie istnieje.".to_owned());
                    self.refresh_folders();
                    return;
                };
                self.select_document_folder(&pdf);
                self.remember_recent_document(&pdf.path);
                self.open_pdf_for_edit(&pdf, context);
            }
            LibraryAction::Rename(path) => {
                let Some(pdf) = self.pdf_by_path(&path) else {
                    self.message = Some("Ten dokument już nie istnieje.".to_owned());
                    self.refresh_folders();
                    return;
                };
                self.rename_pdf_name = pdf
                    .path
                    .file_stem()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or(pdf.name);
                self.rename_pdf_target = Some(path);
            }
            LibraryAction::Move(paths) => {
                self.move_pdf_targets = paths;
                self.move_pdf_destination = None;
                self.move_folder_query.clear();
            }
            LibraryAction::Recycle(paths) => self.delete_pdf_targets = paths,
            LibraryAction::OpenFolderInExplorer(path) => {
                if let Err(error) = open::that_detached(path) {
                    self.message = Some(format!("Nie można otworzyć folderu: {error}"));
                }
            }
            LibraryAction::OpenSettings => {
                self.show_settings = true;
            }
        }
    }

    fn pdf_by_path(&self, path: &std::path::Path) -> Option<PdfInfo> {
        self.library_snapshot
            .pdfs
            .iter()
            .find(|pdf| pdf.path == path)
            .cloned()
    }

    fn select_document_folder(&mut self, pdf: &PdfInfo) {
        let folder = if pdf.folder_path == self.library_root {
            FolderInfo {
                name: "Główna lokalizacja".to_owned(),
                path: self.library_root.clone(),
                pdf_count: self
                    .library_snapshot
                    .pdfs
                    .iter()
                    .filter(|item| item.folder_path == self.library_root)
                    .count(),
                modified_secs: pdf.modified_secs,
            }
        } else {
            self.library_snapshot
                .folders
                .iter()
                .find(|folder| folder.path == pdf.folder_path)
                .cloned()
                .unwrap_or_else(|| FolderInfo {
                    name: pdf.folder_name.clone(),
                    path: pdf.folder_path.clone(),
                    pdf_count: 0,
                    modified_secs: pdf.modified_secs,
                })
        };
        self.open_folder(folder);
    }

    fn apply_pdf_rename(&mut self, source: PathBuf) {
        match rename_pdf(&source, &self.rename_pdf_name) {
            Ok(destination) => {
                self.rename_pdf_target = None;
                self.replace_recent_document_path(&source, &destination);
                self.library_view.replace_path(&source, &destination);
                self.update_snapshot_files(
                    std::slice::from_ref(&source),
                    std::slice::from_ref(&destination),
                );
                self.toast = Some(Toast {
                    text: "Zmieniono nazwę dokumentu.".to_owned(),
                    shown_at: Instant::now(),
                    pdf_path: Some(destination),
                });
                self.refresh_folders();
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn start_pdf_move(&mut self, target_folder: PathBuf) {
        let sources = std::mem::take(&mut self.move_pdf_targets);
        self.move_pdf_destination = None;
        self.move_folder_query.clear();
        match self
            .file_operation_worker
            .request_move(sources, target_folder)
        {
            Ok(()) => self.pending_file_operation = Some(PendingFileOperation::Move),
            Err(error) => self.message = Some(error),
        }
    }

    fn finish_pdf_move(&mut self, report: FileActionReport) {
        let moved_sources: Vec<PathBuf> = report
            .successes
            .iter()
            .map(|success| success.source.clone())
            .collect();
        let mut recent_changed = false;
        for success in &report.successes {
            if let Some(destination) = &success.destination {
                for recent in &mut self.settings.recent_documents {
                    if recent == &success.source {
                        *recent = destination.clone();
                        recent_changed = true;
                    }
                }
            }
        }
        if recent_changed {
            let _ = save_settings(&self.settings);
        }
        let destinations: Vec<PathBuf> = report
            .successes
            .iter()
            .filter_map(|success| success.destination.clone())
            .collect();
        self.update_snapshot_files(&moved_sources, &destinations);
        self.library_view.remove_paths(moved_sources.iter());
        if report.completed_count() > 0 {
            let skipped = if report.skipped_count() > 0 {
                format!(" ({} już było w folderze)", report.skipped_count())
            } else {
                String::new()
            };
            self.toast = Some(Toast {
                text: format!(
                    "Przeniesiono {}{}.",
                    polish_file_count(report.completed_count()),
                    skipped
                ),
                shown_at: Instant::now(),
                pdf_path: None,
            });
        } else if report.skipped_count() > 0 && report.failed_count() == 0 {
            self.toast = Some(Toast {
                text: "Wybrane pliki są już w tym folderze.".to_owned(),
                shown_at: Instant::now(),
                pdf_path: None,
            });
        }
        if let Some(message) = file_action_failure_message("przenieść", &report) {
            self.message = Some(message);
        }
        self.refresh_folders();
    }

    fn start_pdf_recycle(&mut self) {
        let sources = std::mem::take(&mut self.delete_pdf_targets);
        match self.file_operation_worker.request_recycle(sources) {
            Ok(()) => self.pending_file_operation = Some(PendingFileOperation::Recycle),
            Err(error) => self.message = Some(error),
        }
    }

    fn finish_pdf_recycle(&mut self, report: FileActionReport) {
        let removed: Vec<PathBuf> = report
            .successes
            .iter()
            .map(|success| success.source.clone())
            .collect();
        self.update_snapshot_files(&removed, &[]);
        self.library_view.remove_paths(removed.iter());
        if !removed.is_empty() {
            self.settings
                .recent_documents
                .retain(|path| !removed.contains(path));
            let _ = save_settings(&self.settings);
            self.toast = Some(Toast {
                text: format!(
                    "Przeniesiono do Kosza: {}.",
                    polish_file_count(removed.len())
                ),
                shown_at: Instant::now(),
                pdf_path: None,
            });
        }
        if let Some(message) = file_action_failure_message("przenieść do Kosza", &report) {
            self.message = Some(message);
        }
        self.refresh_folders();
    }

    fn poll_file_operation(&mut self, context: &egui::Context) {
        if self.pending_file_operation.is_none() {
            return;
        }
        match self.file_operation_worker.try_recv() {
            Ok(Some(event)) => {
                self.pending_file_operation = None;
                match event {
                    FileOperationEvent::Moved(report) => self.finish_pdf_move(report),
                    FileOperationEvent::Recycled(report) => self.finish_pdf_recycle(report),
                }
            }
            Ok(None) => context.request_repaint_after(Duration::from_millis(100)),
            Err(error) => {
                self.pending_file_operation = None;
                self.message = Some(error);
            }
        }
    }

    fn scan_hub_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        self.poll_camera(context);
        self.poll_pipeline(context);
        if self.overlay.as_ref().is_some_and(OverlayDetector::is_dead) {
            self.overlay = Some(OverlayDetector::start());
        }
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
            let compact_height = ui.available_height() < 460.0;
            let scan_strip_height = if compact_height { 96.0 } else { STRIP_HEIGHT };
            let preview_min_height = if compact_height { 52.0 } else { 80.0 };
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
            ui.horizontal_wrapped(|ui| {
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
                (ui.max_rect().bottom() - scan_strip_height - 10.0)
                    .max(preview_min.y + preview_min_height),
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
        let tile_height = (ui.available_height() - 12.0).clamp(72.0, 140.0);
        let image_height = (tile_height - 28.0).max(44.0);
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
                (ui.available_height() - 70.0).max(180.0),
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
        if close
            || (!self.has_blocking_dialog()
                && context.input(|input| input.key_pressed(egui::Key::Escape)))
        {
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
                    quarter_turns: data.quarter_turns,
                },
                &data.page.jpeg,
                Vec2::new(data.page.width as f32, data.page.height as f32),
                Some(data.texture.clone()),
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

            let compact_height = ui.available_height() < 430.0;
            let strip_height = if compact_height { 88.0 } else { 124.0 };
            let minimum_body_height = if compact_height { 140.0 } else { 220.0 };
            let body_size = Vec2::new(
                ui.available_width(),
                (ui.available_height() - strip_height - 10.0).max(minimum_body_height),
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
                                    ui.add_sized(
                                        [ui.available_width(), CONTROL_HEIGHT],
                                        Button::new("Obróć"),
                                    )
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
                                        [ui.available_width(), CONTROL_HEIGHT],
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
                                    [ui.available_width(), CONTROL_HEIGHT],
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
                                [ui.available_width(), CONTROL_HEIGHT],
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

                            let target = self.cached_save_target();
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
                            let save_enabled = self.can_save() && target.is_ok();
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
                    if !folder_display.is_empty() {
                        ui.label(format!("Folder: {folder_display}"));
                    }
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
                            RichText::new(
                                "Pierwotny folder nie jest dostępny — strony trafią do głównej lokalizacji biblioteki.",
                            )
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
                                session.clear();
                            }
                            self.recovered = None;
                            self.show_restore = false;
                        }
                        if primary_button(ui, "Przywróć").clicked() {
                            if !folder_exists && let Some(recovered) = &mut self.recovered {
                                recovered.folder_path = self.library_root.clone();
                            }
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
                    [360.0, CONTROL_HEIGHT],
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
                                self.upsert_snapshot_folder(&folder);
                                self.open_folder(folder);
                                self.refresh_folders();
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
                    [360.0, CONTROL_HEIGHT],
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
                            Ok(renamed_folder) => {
                                for recent in &mut self.settings.recent_documents {
                                    if let Ok(suffix) = recent.strip_prefix(&folder.path) {
                                        *recent = renamed_folder.path.join(suffix);
                                    }
                                }
                                self.library_view
                                    .replace_prefix(&folder.path, &renamed_folder.path);
                                self.selected_folder = Some(renamed_folder.clone());
                                self.settings.last_folder = Some(renamed_folder.name.clone());
                                self.settings.last_folder_path = Some(renamed_folder.path.clone());
                                self.update_snapshot_folder(&folder, &renamed_folder);
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
                ui.set_min_width(420.0);
                ui.set_max_width(540.0);
                ui.heading("Ustawienia");
                ui.add_space(8.0);
                ui.label(RichText::new("Folder biblioteki").strong());
                let library_path = self.library_root.display().to_string();
                Frame::new()
                    .fill(Color32::from_rgb(246, 248, 251))
                    .stroke(Stroke::new(1.0, Color32::from_gray(218)))
                    .corner_radius(8.0)
                    .inner_margin(10)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(
                                RichText::new(&library_path)
                                    .size(15.0)
                                    .color(Color32::from_gray(65)),
                            )
                            .wrap()
                            .selectable(true),
                        );
                    });
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Otwórz lokalizację").clicked()
                        && let Err(error) = open::that_detached(&self.library_root)
                    {
                        self.message = Some(format!("Nie można otworzyć folderu: {error}"));
                    }
                    if primary_button(ui, "Zmień lokalizację").clicked() {
                        if self.has_active_workflow() {
                            self.show_settings = false;
                            self.message =
                                Some("Najpierw zapisz albo anuluj bieżący dokument.".to_owned());
                        } else if let Some(path) = rfd::FileDialog::new().pick_folder() {
                            if let Err(error) = ensure_library(&path) {
                                self.message = Some(error);
                            } else {
                                self.library_root = path.clone();
                                self.settings.library_root = Some(path.clone());
                                self.settings.last_folder = None;
                                self.settings.last_folder_path = None;
                                self.settings.recent_documents.clear();
                                self.selected_folder = None;
                                self.folders.clear();
                                self.pdfs.clear();
                                self.library_snapshot = LibrarySnapshot {
                                    root: path,
                                    ..LibrarySnapshot::default()
                                };
                                self.library_revision = self.library_revision.saturating_add(1);
                                self.library_view.clear_selection();
                                self.library_restore_pending = false;
                                self.screen = Screen::Library;
                                if let Err(error) = save_settings(&self.settings) {
                                    self.message = Some(error);
                                }
                                self.refresh_folders();
                            }
                        }
                    }
                });
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Gotowe").clicked() {
                        self.show_settings = false;
                    }
                });
            });
        }

        if let Some(source) = self.rename_pdf_target.clone() {
            egui::Modal::new(Id::new("rename-document-modal")).show(context, |ui| {
                ui.heading("Zmień nazwę dokumentu");
                ui.add_space(8.0);
                ui.set_max_width(480.0);
                ui.label("Nowa nazwa:");
                let response = ui.add_sized(
                    [420.0, CONTROL_HEIGHT],
                    egui::TextEdit::singleline(&mut self.rename_pdf_name),
                );
                if context.memory(|memory| memory.focused().is_none()) {
                    response.request_focus();
                }
                ui.label(
                    RichText::new("Rozszerzenie .pdf zostanie dodane automatycznie.")
                        .small()
                        .color(Color32::GRAY),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Anuluj").clicked() {
                        self.rename_pdf_target = None;
                    }
                    let enter = response.has_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if primary_button(ui, "Zapisz nazwę").clicked() || enter {
                        self.apply_pdf_rename(source.clone());
                    }
                });
            });
        }

        if !self.move_pdf_targets.is_empty() {
            let targets = self.move_pdf_targets.clone();
            let folders = self.library_snapshot.folders.clone();
            egui::Modal::new(Id::new("move-documents-modal")).show(context, |ui| {
                ui.heading("Przenieś dokumenty");
                ui.add_space(6.0);
                ui.set_min_width(440.0);
                ui.label(format!("Wybrano: {}", polish_file_count(targets.len())));
                for path in targets.iter().take(3) {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| path.to_string_lossy());
                    ui.label(RichText::new(format!("• {name}")).small());
                }
                if targets.len() > 3 {
                    ui.label(
                        RichText::new(format!("• …oraz {} kolejnych", targets.len() - 3)).small(),
                    );
                }
                ui.add_space(10.0);
                ui.label(RichText::new("Folder docelowy").strong());
                if folders.is_empty() {
                    ui.label("Brak folderów docelowych. Najpierw utwórz folder.");
                } else {
                    ui.add_sized(
                        [ui.available_width(), CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut self.move_folder_query)
                            .hint_text("Szukaj folderu docelowego…"),
                    );
                    let normalized_query = normalize_search_text(&self.move_folder_query);
                    let query_tokens: Vec<&str> = normalized_query.split_whitespace().collect();
                    let mut shown_folders = 0;
                    egui::ScrollArea::vertical()
                        .id_salt("move-document-destinations")
                        .max_height(240.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for folder in &folders {
                                let normalized_name = normalize_search_text(&folder.name);
                                if !query_tokens
                                    .iter()
                                    .all(|token| normalized_name.contains(token))
                                {
                                    continue;
                                }
                                shown_folders += 1;
                                let all_already_here = targets
                                    .iter()
                                    .all(|source| source.parent() == Some(folder.path.as_path()));
                                let selected =
                                    self.move_pdf_destination.as_ref() == Some(&folder.path);
                                let label = format!("{}  ({} PDF)", folder.name, folder.pdf_count);
                                let response = ui
                                    .add_enabled_ui(!all_already_here, |ui| {
                                        ui.selectable_label(selected, label)
                                    })
                                    .inner;
                                if response.clicked() {
                                    self.move_pdf_destination = Some(folder.path.clone());
                                }
                                if all_already_here {
                                    response.on_disabled_hover_text("Pliki są już w tym folderze.");
                                }
                            }
                        });
                    if shown_folders == 0 {
                        ui.label(
                            RichText::new("Brak pasujących folderów.")
                                .small()
                                .color(Color32::GRAY),
                        );
                    }
                }
                ui.label(
                    RichText::new("Jeśli nazwa już istnieje, aplikacja zachowa oba pliki.")
                        .small()
                        .color(Color32::GRAY),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Anuluj").clicked() {
                        self.move_pdf_targets.clear();
                        self.move_pdf_destination = None;
                        self.move_folder_query.clear();
                    }
                    let destination = self.move_pdf_destination.clone();
                    if ui
                        .add_enabled_ui(destination.is_some(), |ui| primary_button(ui, "Przenieś"))
                        .inner
                        .clicked()
                        && let Some(destination) = destination
                    {
                        self.start_pdf_move(destination);
                    }
                });
            });
        }

        if !self.delete_pdf_targets.is_empty() {
            let targets = self.delete_pdf_targets.clone();
            egui::Modal::new(Id::new("delete-document-modal")).show(context, |ui| {
                ui.heading("Przenieść do Kosza?");
                ui.add_space(8.0);
                ui.set_max_width(480.0);
                ui.label(format!("Wybrano: {}", polish_file_count(targets.len())));
                for path in targets.iter().take(3) {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| path.to_string_lossy());
                    ui.label(RichText::new(format!("• {name}")).small());
                }
                if targets.len() > 3 {
                    ui.label(
                        RichText::new(format!("• …oraz {} kolejnych", targets.len() - 3)).small(),
                    );
                }
                ui.add_space(8.0);
                ui.label("Pliki będzie można odzyskać z systemowego Kosza.");
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Anuluj").clicked() {
                        self.delete_pdf_targets.clear();
                    }
                    if ui
                        .button(RichText::new("Przenieś do Kosza").color(Color32::DARK_RED))
                        .clicked()
                    {
                        self.start_pdf_recycle();
                    }
                });
            });
        }

        if let Some(operation) = self.pending_file_operation {
            let (title, detail) = match operation {
                PendingFileOperation::Move => (
                    "Przenoszenie dokumentów…",
                    "Zachowywanie obu plików przy zbieżnych nazwach.",
                ),
                PendingFileOperation::Recycle => (
                    "Przenoszenie do Kosza…",
                    "Pliki pozostaną możliwe do odzyskania.",
                ),
            };
            egui::Modal::new(Id::new("library-file-operation-modal")).show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.heading(title);
                });
                ui.label(detail);
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
        let has_unsaved_scan = !can_leave_scan_without_confirmation(
            self.slots.is_empty(),
            self.editing_target.is_some(),
            self.edit_dirty,
        );
        if close_requested && has_unsaved_scan && !self.allow_exit {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_exit_confirm = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        self.poll_file_operation(&context);
        self.poll_library(&context);
        self.poll_session_errors();
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
    style.spacing.button_padding = Vec2::new(14.0, 9.0);
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.interact_size.y = CONTROL_HEIGHT;
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
    style
        .text_styles
        .insert(egui::TextStyle::Small, FontId::proportional(14.0));
    context.set_style_of(egui::Theme::Light, style);
}

fn page_container(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .inner_margin(Margin::symmetric(20, 12))
        .show(ui, |ui| add_contents(ui));
}

fn two_sided<Left, Right>(
    ui: &mut egui::Ui,
    height: f32,
    add_left: impl FnOnce(&mut egui::Ui) -> Left,
    add_right: impl FnOnce(&mut egui::Ui) -> Right,
) -> (Left, Right) {
    egui::containers::Sides::new()
        .height(height)
        .shrink_left()
        .truncate()
        .show(ui, add_left, add_right)
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        Button::new(RichText::new(text).strong().color(Color32::WHITE))
            .fill(BLUE)
            .corner_radius(9.0)
            .min_size(Vec2::new(0.0, CONTROL_HEIGHT)),
    )
}

fn file_action_failure_message(action: &str, report: &FileActionReport) -> Option<String> {
    if report.failures.is_empty() {
        return None;
    }
    let mut message = format!(
        "Nie udało się {action} {} z {} plików.",
        report.failed_count(),
        report.completed_count() + report.failed_count() + report.skipped_count()
    );
    for failure in report.failures.iter().take(3) {
        let name = failure
            .source
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_else(|| failure.source.to_string_lossy());
        message.push_str(&format!("\n• {name}: {}", failure.error));
    }
    if report.failures.len() > 3 {
        message.push_str(&format!(
            "\n• …oraz {} kolejnych.",
            report.failures.len() - 3
        ));
    }
    Some(message)
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
    crate::atomic_file::content_fingerprint(path)
        .map_err(|error| format!("Nie można sprawdzić pliku źródłowego: {error}"))
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

/// Film-strip texture for a page, rotated for display by its quarter turns.
fn strip_texture(
    context: &egui::Context,
    id: u64,
    revision: u64,
    page: &ScannedPage,
    quarter_turns: u8,
) -> egui::TextureHandle {
    let rotated;
    let source = if quarter_turns.is_multiple_of(4) {
        &page.review_image
    } else {
        rotated = rotate_rgb(&page.review_image, quarter_turns);
        &rotated
    };
    context.load_texture(
        format!("strona-{id}-r{revision}-t{quarter_turns}"),
        rgb_to_color_image(source),
        TextureOptions::LINEAR,
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

fn polish_file_count(count: usize) -> String {
    let word = if count == 1 {
        "plik"
    } else if (2..=4).contains(&(count % 10)) && !(12..=14).contains(&(count % 100)) {
        "pliki"
    } else {
        "plików"
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
