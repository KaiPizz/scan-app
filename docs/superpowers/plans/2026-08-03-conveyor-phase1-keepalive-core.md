# Conveyor Phase 1 — Keep-Alive Capture Core: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the per-page `Scan → Crop → Review` loop with a single ScanHub screen where the camera never stops, pages process on a background worker, and saving keeps the conveyor running.

**Architecture:** New `pipeline.rs` worker thread does detect+warp+encode off the UI thread and reports `PageReady`/`PageFailed` events tagged by opaque page ids. `app.rs` replaces three screens with ScanHub (live preview + film strip of `PageSlot`s) and a streamlined save dialog + toast. `camera.rs` is untouched — the app simply never stops it between captures.

**Tech Stack:** Rust edition 2024, eframe/egui 0.35, image/imageproc, printpdf, nokhwa (unchanged). No new dependencies in this phase.

**Spec:** `docs/superpowers/specs/2026-08-03-conveyor-scan-workflow-design.md` (§10 Phase 1)

## Global Constraints

- Repo of record: `D:\scan-app` on machine `gm` (Windows, SSH alias `gm`). Local editing copy: `/tmp/claude-1000/-var-www-www-enail/fb978f12-cae6-42ac-87f4-1e02f098756f/scratchpad/scan-app` (referred to as `<LOCAL>`).
- Cargo is NOT in gm's ssh PATH — always call `%USERPROFILE%\.cargo\bin\cargo.exe`.
- Tests MUST run `--release` (debug-mode warp of 13 MP frames is minutes-slow).
- All UI strings in Polish; follow existing tone (short, imperative).
- Camera discovery/open logic (`camera.rs`), storage naming rules (`storage.rs`), PDF writer internals, and Library/Folder screens must not change (exception: `save_pdf` signature, Task 2).
- Commit identity on gm: repo-local `user.name=KaiPizz`, `user.email=butikwow.pl@gmail.com` (set in Task 1 Step 0).
- Command templates (run from Netcup):
  - SYNC: `scp <LOCAL>/src/*.rs gm:"D:/scan-app/src/"`
  - TEST: `ssh gm "cd /d D:\scan-app && %USERPROFILE%\.cargo\bin\cargo.exe test --release 2>&1" | tail -40`
  - BUILD: `ssh gm "cd /d D:\scan-app && %USERPROFILE%\.cargo\bin\cargo.exe build --release 2>&1" | tail -20`
  - COMMIT: `ssh gm "cd /d D:\scan-app && git add -A && git commit -m \"<msg>\""`
- Definition of done per task: TEST shows `test result: ok` for every suite, BUILD ends `Finished`, then COMMIT.

## File Structure

- **Create** `src/pipeline.rs` — background processing worker: one thread, FIFO jobs, events out. No egui/nokhwa imports.
- **Modify** `src/main.rs` — register `mod pipeline;`.
- **Modify** `src/document.rs` — `save_pdf` takes `&[&ScannedPage]` (avoid cloning ~5 MB/page at save time); its test updated.
- **Modify** `src/app.rs` — screen enum {Library, Folder, ScanHub}; `PageSlot` model; ScanHub UI (preview + film strip + selection row); keep-alive capture; save dialog rework; cancel-confirm; keyboard; toast. Old `Crop`/`Review` screens and their helpers deleted.
- `src/camera.rs`, `src/storage.rs` — untouched.

---

### Task 1: Processing pipeline (`pipeline.rs`)

**Files:**
- Create: `src/pipeline.rs`
- Modify: `src/main.rs` (add `mod pipeline;`)

**Interfaces:**
- Consumes: `document::{detect_document_corners, process_page, CropPoint, ScannedPage}`, `image::RgbImage`.
- Produces (used by Task 2):
  - `ProcessingPipeline::start() -> ProcessingPipeline`
  - `ProcessingPipeline::try_submit(&self, id: u64, frame: Arc<RgbImage>) -> bool` (false = queue full/closed)
  - `ProcessingPipeline::try_event(&self) -> Option<PipelineEvent>`
  - `ProcessingPipeline::shutdown(&mut self)` (also runs on Drop; aborts queued jobs, joins)
  - `enum PipelineEvent { PageReady { id: u64, page: ScannedPage, original_jpeg: Vec<u8>, corners: [CropPoint; 4] }, PageFailed { id: u64, original_jpeg: Vec<u8>, error: String } }`
  - `pub const QUEUE_CAPACITY: usize = 8;`

- [ ] **Step 0: One-time repo setup on gm**

```bash
ssh gm "cd /d D:\scan-app && git config user.name KaiPizz && git config user.email butikwow.pl@gmail.com && git config user.name"
```
Expected output: `KaiPizz`

- [ ] **Step 1: Write the failing tests**

Create `src/pipeline.rs` containing ONLY the test module first is impossible (tests need the types), so create the full file skeleton with `todo!()` bodies plus the tests below, OR write the complete file in one go and rely on the red step being "module not registered". Pragmatic TDD for a new module: write tests + full implementation in one file, but FIRST run the test with `mod pipeline;` absent from `main.rs` to confirm the test harness fails to find it (red), then register the module (green). The test code:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{A4_HEIGHT_PX, A4_WIDTH_PX};
    use image::Rgb;
    use std::time::{Duration, Instant};

    fn white_document_frame(width: u32, height: u32) -> RgbImage {
        let mut image = RgbImage::from_pixel(width, height, Rgb([25, 25, 25]));
        let margin_x = width / 5;
        let margin_y = height / 5;
        for y in margin_y..height - margin_y {
            for x in margin_x..width - margin_x {
                image.put_pixel(x, y, Rgb([245, 245, 245]));
            }
        }
        image
    }

    fn collect_events(
        pipeline: &ProcessingPipeline,
        count: usize,
        timeout: Duration,
    ) -> Vec<PipelineEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        while events.len() < count && Instant::now() < deadline {
            match pipeline.try_event() {
                Some(event) => events.push(event),
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        events
    }

    #[test]
    fn processes_pages_in_submit_order_with_caller_ids() {
        let pipeline = ProcessingPipeline::start();
        assert!(pipeline.try_submit(7, Arc::new(white_document_frame(400, 300))));
        assert!(pipeline.try_submit(9, Arc::new(white_document_frame(300, 400))));
        let events = collect_events(&pipeline, 2, Duration::from_secs(120));
        assert_eq!(events.len(), 2, "worker nie odesłał dwóch zdarzeń");
        let ids: Vec<u64> = events
            .iter()
            .map(|event| match event {
                PipelineEvent::PageReady { id, .. } => *id,
                PipelineEvent::PageFailed { id, .. } => *id,
            })
            .collect();
        assert_eq!(ids, vec![7, 9]);
        for event in events {
            match event {
                PipelineEvent::PageReady {
                    page,
                    original_jpeg,
                    corners,
                    ..
                } => {
                    assert_eq!((page.width, page.height), (A4_WIDTH_PX, A4_HEIGHT_PX));
                    assert!(original_jpeg.starts_with(&[0xFF, 0xD8]), "oryginał nie jest JPEG");
                    for corner in corners {
                        assert!((0.0..=1.0).contains(&corner.x));
                        assert!((0.0..=1.0).contains(&corner.y));
                    }
                }
                PipelineEvent::PageFailed { error, .. } => {
                    panic!("nieoczekiwany błąd przetwarzania: {error}")
                }
            }
        }
    }

    #[test]
    fn shutdown_aborts_queued_jobs_promptly() {
        let mut pipeline = ProcessingPipeline::start();
        for id in 0..6 {
            let _ = pipeline.try_submit(id, Arc::new(white_document_frame(400, 300)));
        }
        let started = Instant::now();
        pipeline.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "shutdown czekał na całą kolejkę zamiast ją porzucić"
        );
    }
}
```

- [ ] **Step 2: Write the implementation (same file, above the tests)**

```rust
use crate::document::{CropPoint, ScannedPage, detect_document_corners, process_page};
use image::RgbImage;
use image::codecs::jpeg::JpegEncoder;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub const QUEUE_CAPACITY: usize = 8;
const ORIGINAL_JPEG_QUALITY: u8 = 88;

pub enum PipelineEvent {
    PageReady {
        id: u64,
        page: ScannedPage,
        original_jpeg: Vec<u8>,
        corners: [CropPoint; 4],
    },
    PageFailed {
        id: u64,
        original_jpeg: Vec<u8>,
        error: String,
    },
}

struct Job {
    id: u64,
    frame: Arc<RgbImage>,
}

pub struct ProcessingPipeline {
    jobs: Option<SyncSender<Job>>,
    events: Receiver<PipelineEvent>,
    worker: Option<JoinHandle<()>>,
    abort: Arc<AtomicBool>,
}

impl ProcessingPipeline {
    pub fn start() -> Self {
        let (job_sender, job_receiver) = sync_channel::<Job>(QUEUE_CAPACITY);
        let (event_sender, event_receiver) = channel();
        let abort = Arc::new(AtomicBool::new(false));
        let worker_abort = Arc::clone(&abort);
        let worker =
            thread::spawn(move || worker_loop(&job_receiver, &event_sender, &worker_abort));
        Self {
            jobs: Some(job_sender),
            events: event_receiver,
            worker: Some(worker),
            abort,
        }
    }

    pub fn try_submit(&self, id: u64, frame: Arc<RgbImage>) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.try_send(Job { id, frame }).is_ok())
    }

    pub fn try_event(&self) -> Option<PipelineEvent> {
        self.events.try_recv().ok()
    }

    pub fn shutdown(&mut self) {
        self.abort.store(true, Ordering::Release);
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ProcessingPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(
    jobs: &Receiver<Job>,
    events: &Sender<PipelineEvent>,
    abort: &Arc<AtomicBool>,
) {
    while let Ok(job) = jobs.recv() {
        if abort.load(Ordering::Acquire) {
            return;
        }
        if events.send(process_job(&job)).is_err() {
            return;
        }
    }
}

fn process_job(job: &Job) -> PipelineEvent {
    let corners = detect_document_corners(&job.frame);
    let mut original_jpeg = Vec::new();
    if JpegEncoder::new_with_quality(&mut original_jpeg, ORIGINAL_JPEG_QUALITY)
        .encode_image(job.frame.as_ref())
        .is_err()
    {
        original_jpeg.clear();
    }
    match process_page(&job.frame, corners) {
        Ok(page) => PipelineEvent::PageReady {
            id: job.id,
            page,
            original_jpeg,
            corners,
        },
        Err(error) => PipelineEvent::PageFailed {
            id: job.id,
            original_jpeg,
            error,
        },
    }
}
```

- [ ] **Step 3: Red — run tests WITHOUT registering the module**

SYNC (`scp <LOCAL>/src/pipeline.rs gm:"D:/scan-app/src/"`), do NOT touch `main.rs` yet, then TEST.
Expected: compile succeeds but `pipeline` tests are absent from output (module not compiled) — confirms the harness genuinely picks the module up only via Step 4. (`cargo` does not error on unregistered files.)

- [ ] **Step 4: Green — register module**

In `<LOCAL>/src/main.rs` add `mod pipeline;` after `mod document;`:

```rust
mod app;
mod camera;
mod document;
mod pipeline;
mod storage;
```

SYNC both files, TEST.
Expected: `running 2 tests` for pipeline (order test + shutdown test), `test result: ok` everywhere. A `dead_code` warning about unused pipeline items is expected until Task 2 consumes them.

- [ ] **Step 5: Commit**

```bash
ssh gm "cd /d D:\scan-app && git add -A && git commit -m \"feat: background page-processing pipeline\""
```

---

### Task 2: ScanHub rewrite in `app.rs` (+ `save_pdf` signature)

**Files:**
- Modify: `src/app.rs` (major rewrite of scan flow; Library/Folder UIs untouched)
- Modify: `src/document.rs:273` (`save_pdf` takes `&[&ScannedPage]`) and its test `writes_a_valid_pdf`

**Interfaces:**
- Consumes: `pipeline::{ProcessingPipeline, PipelineEvent}` (Task 1 signatures), existing `camera::CameraController`, `document::{rotate_page_clockwise, save_pdf, CropPoint, ScannedPage}`, `storage::*`.
- Produces (used by Task 3): app fields `slots: Vec<SlotEntry>`, `selected_slot: Option<usize>`, `pending_jobs: usize`, `pipeline: Option<ProcessingPipeline>`, methods `capture()`, `can_save() -> bool`, `save_current_document()`, `request_cancel_scan()`, `abandon_scan()`; `Screen::ScanHub`.

- [ ] **Step 1: `save_pdf` signature (document.rs)**

Change line 273 `pub fn save_pdf(path: &Path, pages: &[ScannedPage])` to:

```rust
pub fn save_pdf(path: &Path, pages: &[&ScannedPage]) -> Result<(), String> {
```

Inside, the loop `for page in pages` keeps working (`page` becomes `&&ScannedPage`, auto-deref covers field access). In test `writes_a_valid_pdf` change `save_pdf(&path, &[page])` to `save_pdf(&path, &[&page])`.

- [ ] **Step 2: Replace the screen/state model (app.rs)**

Replace the `Screen` enum and the scan-related fields of `DocumentScannerApp`:

```rust
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
    Failed { original_jpeg: Vec<u8>, error: String },
}

struct SlotEntry {
    id: u64,
    slot: PageSlot,
}

struct Toast {
    text: String,
    shown_at: Instant,
}
```

Field surgery on `DocumentScannerApp`:

REMOVE: `crop_image`, `crop_texture`, `crop_points`, `crop_busy`, `pages`, `page_textures`, `selected_page`, `show_saved`, `saved_path`, `show_delete_confirm` stays (reused), everything else stays.

ADD:

```rust
    slots: Vec<SlotEntry>,
    selected_slot: Option<usize>,
    next_page_id: u64,
    pending_jobs: usize,
    pipeline: Option<ProcessingPipeline>,
    toast: Option<Toast>,
    show_cancel_confirm: bool,
```

Imports at top of app.rs: remove `detect_document_corners, process_page, rotate_page_clockwise, save_pdf` → keep `CropPoint, ScannedPage, rotate_page_clockwise, save_pdf`; add `use crate::pipeline::{PipelineEvent, ProcessingPipeline};` and `use std::time::Instant;` (Duration already imported). Initialize the new fields in `new()` (empty vec, `None`, `0`, `false`).

- [ ] **Step 3: Rewire scan lifecycle methods**

Replace `begin_scan`, `capture`, delete `rotate_crop_source`, `accept_crop`; adjust `cancel_scan` → split into `request_cancel_scan`/`abandon_scan`; adapt `rotate_selected_page`, `delete_selected_page`, `move_selected_page`, `save_current_document`; add `poll_pipeline`, `can_save`. Exact code:

```rust
    fn begin_scan(&mut self) {
        self.slots.clear();
        self.selected_slot = None;
        self.pending_jobs = 0;
        self.filename.clear();
        self.pipeline = Some(ProcessingPipeline::start());
        self.start_camera();
    }
```

`start_camera` keeps its body but sets `self.screen = Screen::ScanHub;` (was `Screen::Scan`). `poll_camera`'s repaint condition becomes `if self.screen == Screen::ScanHub`.

```rust
    fn capture(&mut self) {
        let Some(frame) = self
            .camera
            .as_ref()
            .and_then(CameraController::latest_full_image)
        else {
            self.message = Some("Poczekaj, aż pojawi się obraz z kamery.".to_owned());
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
        } else {
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
        let mut pages = Vec::with_capacity(self.slots.len());
        for entry in &self.slots {
            match &entry.slot {
                PageSlot::Ready(data) => pages.push(&data.page),
                _ => {
                    self.message =
                        Some("Usuń strony z błędem (⚠) przed zapisem.".to_owned());
                    return;
                }
            }
        }
        let path = match unique_pdf_path(&folder.path, &self.filename) {
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
        self.slots.clear();
        self.selected_slot = None;
        self.pending_jobs = 0;
        self.screen = Screen::Folder;
        self.refresh_pdfs();
    }
```

Note: `save_current_document` intentionally keeps the camera and pipeline running and stays on ScanHub — that is the conveyor behavior. `borrow` detail: the `pages` vec of `&ScannedPage` borrows `self.slots`, so the `self.message = …` inside the loop must return immediately (it does) — the code above compiles because the borrow ends at each `return`. If the borrow checker complains about `self.message` assignment while `folder`/`pages` are borrowed, hoist `let folder_path = folder.path.clone();` before building `pages` and use `unique_pdf_path(&folder_path, …)`.

- [ ] **Step 4: Delete `crop_ui` + `review_ui`, write `scan_hub_ui`**

Delete functions `crop_ui`, `review_ui`, `constrain_corner`. Replace `scan_ui` with `scan_hub_ui`:

```rust
    fn scan_hub_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        self.poll_camera(context);
        self.poll_pipeline(context);
        page_container(ui, |ui| {
            let camera_ready = self.camera_ready;
            let camera_status = self.camera_status.clone();
            let page_count_text = polish_page_count(self.slots.len());
            let (cancel, ()) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut cancel = false;
                    ui.horizontal(|ui| {
                        cancel = ui.button("Anuluj dokument").clicked();
                        ui.add_space(10.0);
                        ui.heading(format!("Skanowanie · {page_count_text}"));
                    });
                    cancel
                },
                |ui| {
                    let color = if camera_ready {
                        Color32::DARK_GREEN
                    } else {
                        Color32::DARK_GRAY
                    };
                    ui.label(RichText::new(camera_status).color(color));
                },
            );
            if cancel {
                self.request_cancel_scan();
                return;
            }
            ui.add_space(10.0);

            // Controls row: capture + save.
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
                self.capture();
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
                self.show_save = true;
            }
            if !self.camera_ready && controls_ui.button("Spróbuj ponownie").clicked() {
                self.start_camera();
            }
            ui.add_space(8.0);

            // Selection row (visible when a page is selected).
            if let Some(index) = self.selected_slot {
                if index >= self.slots.len() {
                    self.selected_slot = None;
                } else {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Strona {} z {}",
                                index + 1,
                                self.slots.len()
                            ))
                            .strong(),
                        );
                        let can_left = index > 0;
                        let can_right = index + 1 < self.slots.len();
                        if ui.add_enabled(can_left, Button::new("← W lewo")).clicked() {
                            self.move_selected_page(-1);
                        }
                        if ui.add_enabled(can_right, Button::new("W prawo →")).clicked() {
                            self.move_selected_page(1);
                        }
                        let rotatable = matches!(
                            self.slots.get(index).map(|entry| &entry.slot),
                            Some(PageSlot::Ready(_))
                        );
                        if ui.add_enabled(rotatable, Button::new("Obróć")).clicked() {
                            self.rotate_selected_page(context);
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

            // Preview area (bottom 170 px reserved for the film strip).
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
                    painter.image(
                        texture.id(),
                        Rect::from_center_size(image_bounds.center(), size),
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
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

            // Film strip.
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
                                    PageSlot::Processing => {
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
                                        ui.label(
                                            RichText::new("Błąd")
                                                .color(Color32::DARK_RED),
                                        );
                                    }
                                });
                            });
                        if frame_response
                            .response
                            .interact(Sense::click())
                            .clicked()
                        {
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
```

- [ ] **Step 5: Update `ui()` dispatch, exit logic, dialogs**

In `impl eframe::App`: `logic()` unsaved check becomes:

```rust
        let has_unsaved_scan = !self.slots.is_empty();
```

`ui()` match:

```rust
            match self.screen {
                Screen::Library => self.library_ui(ui),
                Screen::Folder => self.folder_ui(ui),
                Screen::ScanHub => self.scan_hub_ui(ui, &context),
            }
```

In `dialogs()`: DELETE the whole `show_saved` window. Update the delete-confirm window body (behavior no longer returns to camera on last page — ScanHub already shows the camera):

```rust
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
```

`on_exit` unchanged. The old `cancel_scan` calls inside `dialogs`/elsewhere: none remain (only ScanHub's button calls `request_cancel_scan`). The save dialog (`show_save`) stays AS-IS in this task (rework lands in Task 3); its "Zapisz PDF" button already calls the new `save_current_document`.

- [ ] **Step 6: Build + tests + commit**

SYNC (`app.rs`, `document.rs`, plus any file touched), TEST, BUILD.
Expected: all suites `test result: ok` (document tests updated), build `Finished`. Compile errors about unused helpers (`constrain_corner` etc.) mean a deletion was missed — delete them.

```bash
ssh gm "cd /d D:\scan-app && git add -A && git commit -m \"feat: ScanHub conveyor screen with keep-alive camera and film strip\""
```

---

### Task 3: Save-flow polish, confirmations, keyboard, toast

**Files:**
- Modify: `src/app.rs` (dialogs + input handling)

**Interfaces:**
- Consumes: Task 2 fields/methods (`can_save`, `capture`, `request_cancel_scan`, `abandon_scan`, `toast`, `show_cancel_confirm`).
- Produces: final Phase-1 UX. New app field `save_dialog_needs_focus: bool` (init `false`).

- [ ] **Step 1: Global keys in ScanHub**

At the top of `ui()` in `impl eframe::App` (before drawing), after `let context = …`:

```rust
        let dialog_open = self.show_save
            || self.show_cancel_confirm
            || self.show_delete_confirm
            || self.show_settings
            || self.show_new_folder
            || self.show_rename_folder
            || self.show_exit_confirm
            || self.message.is_some();
        if self.screen == Screen::ScanHub && !dialog_open {
            let (space, enter) = context.input(|input| {
                (
                    input.key_pressed(egui::Key::Space),
                    input.key_pressed(egui::Key::Enter),
                )
            });
            if space {
                self.capture();
            }
            if enter && self.can_save() {
                self.save_dialog_needs_focus = true;
                self.show_save = true;
            }
        }
```

Also set `self.save_dialog_needs_focus = true;` at the ScanHub save-button click in `scan_hub_ui` (Task 2 Step 4 added that click → `self.show_save = true;` — extend it).

- [ ] **Step 2: Rework the save dialog**

Replace the `show_save` window body in `dialogs()`:

```rust
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
                    if back_clicked
                        || ui.input(|input| input.key_pressed(egui::Key::Escape))
                    {
                        self.show_save = false;
                    }
                    if submitted || save_clicked {
                        self.save_current_document();
                    }
                });
        }
```

- [ ] **Step 3: Cancel-document confirmation dialog**

Add to `dialogs()`:

```rust
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
                            .button(
                                RichText::new("Anuluj dokument").color(Color32::DARK_RED),
                            )
                            .clicked()
                        {
                            self.show_cancel_confirm = false;
                            self.abandon_scan();
                        }
                    });
                });
        }
```

- [ ] **Step 4: Toast rendering**

At the end of `dialogs()` (after the `message` window):

```rust
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
                            ui.label(
                                RichText::new(&toast.text)
                                    .color(Color32::WHITE)
                                    .size(16.0),
                            );
                        });
                });
            context.request_repaint_after(Duration::from_millis(250));
        }
```

- [ ] **Step 5: "Wróć do folderu" escape hatch**

In `scan_hub_ui` header (left side, next to "Anuluj dokument"), add a back button enabled only when there is nothing to lose:

```rust
                        let back = ui
                            .add_enabled(self.slots.is_empty(), Button::new("Wróć do folderu"))
                            .clicked();
```

Thread it out of `two_sided` alongside `cancel` (change the closure return to `(cancel, back)` and destructure `let ((cancel, back), ()) = two_sided(…)`). After the `if cancel { … }` block:

```rust
            if back {
                self.abandon_scan();
                return;
            }
```

- [ ] **Step 6: Build + tests + commit**

SYNC, TEST, BUILD — all green, then:

```bash
ssh gm "cd /d D:\scan-app && git add -A && git commit -m \"feat: streamlined save flow, cancel confirm, keyboard, toast\""
```

---

### Task 4: Verification pass on gm

**Files:** none (verification + fixes only)

- [ ] **Step 1: Full clean check**

```bash
ssh gm "cd /d D:\scan-app && %USERPROFILE%\.cargo\bin\cargo.exe fmt 2>&1 && %USERPROFILE%\.cargo\bin\cargo.exe test --release 2>&1" | tail -30
ssh gm "cd /d D:\scan-app && %USERPROFILE%\.cargo\bin\cargo.exe build --release 2>&1" | tail -5
```

Expected: fmt silent, every suite `test result: ok`, build `Finished`. If fmt changed files → include in the smoke-fix commit.

- [ ] **Step 2: Launch on the owner's desktop**

```bash
ssh gm "schtasks /create /tn scanapp-launch /tr D:\scan-app\target\release\skaner-dokumentow.exe /sc once /st 23:59 /it /f && schtasks /run /tn scanapp-launch && timeout /t 3 >nul & tasklist | findstr /i skaner && schtasks /delete /tn scanapp-launch /f"
```

- [ ] **Step 3: Owner manual smoke checklist (report results in chat)**

1. Folder → „Nowy skan” → camera preview appears, capture 5 pages with Space rapid-fire — no waiting between shots, ⌛ thumbnails turn into images while you keep shooting.
2. Click a thumbnail → select/rotate/move/delete (delete asks for confirmation).
3. Enter → filename input already focused → type → Enter → green toast, strip clears, camera still live; immediately scan a second document and save it.
4. „Anuluj dokument” with pages present → confirmation appears; confirm returns to Folder.
5. Close window mid-scan with unsaved pages → „Niezapisany dokument” confirmation still works.
6. Both saved PDFs open correctly from the folder list.

- [ ] **Step 4: Fix-and-commit loop**

Any failed checklist item: fix locally → SYNC → TEST/BUILD → relaunch (Step 2) → re-verify → commit as `fix: <issue>`.

---

## Self-Review Notes (done at planning time)

- Spec §10 Phase-1 items all mapped: keep-alive (T2 S3), Space (T3 S1), film strip (T2 S4), pipeline+placeholders (T1, T2), save rework+toast (T3 S2/S4), cancel-confirm fix (T3 S3). Reorder/rotate/delete carried over so Phase 1 has no feature regression vs old Review screen.
- Known intentional deviations, documented for later phases: `rotate_selected_page` still blocks the UI ~0.5–1 s (rare user action; moves to the background re-apply path with PageEditor in Phase 3); `corners`/`original_jpeg` are unused until Phase 3/4 (`PageData` carries them now so the pipeline API doesn't change later); thumbnail click only selects (PageEditor is Phase 3).
- Type consistency checked: `PipelineEvent` id-based (u64) everywhere; `save_pdf(&path, &[&ScannedPage])` matches all three call sites (app save, document test).
