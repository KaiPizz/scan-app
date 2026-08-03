# Conveyor Phases 2–4 — Auto-Capture, PageEditor, Session Recovery: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the conveyor design: hands-free auto-capture with live crop overlay and beep (Phase 2), per-page crop editor working on the original frame (Phase 3), and crash-safe session recovery (Phase 4).

**Architecture:** `autocapture.rs` is a pure, clock-injected state machine fed by preview frames on the UI thread. A tiny `OverlayDetector` helper thread runs corner detection ~3 Hz for the live quad. `pipeline.rs` gains a re-process job variant (user-set corners, skips detect). `session.rs` persists processed page JPEGs by page id + a RON manifest; app rewrites incrementally on the UI thread.

**Tech Stack:** as Phase 1 + new dependency `windows-sys 0.59` (feature `Win32_UI_WindowsAndMessaging`, for `MessageBeep`).

**Spec:** `docs/superpowers/specs/2026-08-03-conveyor-scan-workflow-design.md` (§5 state machine, §4 overlay, §3 PageEditor, §6 recovery)

## Global Constraints

Same as Phase 1 plan (`2026-08-03-conveyor-phase1-keepalive-core.md`): repo of record `gm:D:\scan-app` branch `feat/conveyor-phase1-20260803`, local editing copy `<LOCAL>` = scratchpad clone, cargo via `%USERPROFILE%\.cargo\bin\cargo.exe`, tests `--release`, Polish UI strings, SYNC/TEST/BUILD/COMMIT command templates unchanged.

**Documented deviations from spec (owner-visible improvements, keep):**
1. First-frame behavior: instead of "first page triggers like any page", the machine **baselines on the first frame** (fingerprint := scene at start, state := Cooldown). Prevents auto-shooting the empty desk right after camera start; the operator places page 1 → novelty fires normally.
2. Thumbnail single-click **selects**; the editor opens via an explicit „Popraw kadr" button in the selection row (prevents accidental editor opens mid-conveyor).
3. Session files are written on the UI thread as slot events are applied (10–20 ms/page), not inside the pipeline worker — simpler ownership, same guarantee.
4. Auto-capture is gated off whenever ANY dialog is open (not only the editor) — otherwise a settle during the save dialog would append pages to the document being saved.

## File Structure

- **Create** `src/autocapture.rs` — state machine + Polish hints + unit tests. No egui/camera imports; `image::RgbImage` in, `FeedResult` out.
- **Create** `src/overlay.rs` — `OverlayDetector` helper thread (latest-wins input mailbox → `Option<[CropPoint;4]>` output).
- **Create** `src/session.rs` — `SessionStore` (begin/write/remove/reorder/load/clear) + tests.
- **Modify** `src/pipeline.rs` — `Job` enum {New, Reprocess}; new events `ReprocessDone`/`ReprocessFailed`; `submit_reprocess`.
- **Modify** `src/document.rs` — add `pub fn page_from_jpeg_bytes(jpeg: Vec<u8>) -> Result<ScannedPage, String>` (recovery rebuild).
- **Modify** `src/app.rs` — auto toggle + hint + beep; overlay quad on preview; PageEditor state/UI; `PageSlot::Reprocessing`; session hooks + restore dialog.
- **Modify** `src/main.rs` — register new modules.
- **Modify** `Cargo.toml` — add windows-sys.

---

### Task 1 (Phase 2): `autocapture.rs` state machine

**Files:**
- Create: `src/autocapture.rs`
- Modify: `src/main.rs` (add `mod autocapture;`)

**Interfaces:**
- Consumes: `image::RgbImage` (any size; downscaled internally), `std::time::Instant` (injected clock).
- Produces (used by Task 2):
  - `AutoCapture::new() -> AutoCapture` (enabled, state Baseline)
  - `AutoCapture::set_enabled(&mut self, on: bool)` / `enabled(&self) -> bool`
  - `AutoCapture::feed(&mut self, preview: &RgbImage, now: Instant) -> FeedResult` (`enum FeedResult { None, Trigger }`)
  - `AutoCapture::note_manual_capture(&mut self)` — fingerprint current scene, enter Cooldown
  - `AutoCapture::hint(&self) -> &'static str` — Polish status line
  - Tunables: `MOTION_MAX: f32 = 2.5`, `NOVELTY_MIN: f32 = 12.0`, `STABLE_MS: u64 = 700`

- [ ] **Step 1: Write the file (implementation + tests)**

```rust
use image::{RgbImage, imageops};
use std::time::{Duration, Instant};

pub const MOTION_MAX: f32 = 2.5;
pub const NOVELTY_MIN: f32 = 12.0;
pub const STABLE_MS: u64 = 700;
const SAMPLE_WIDTH: u32 = 160;
const SAMPLE_HEIGHT: u32 = 120;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Baseline,
    Armed,
    Settling,
    Cooldown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeedResult {
    None,
    Trigger,
}

pub struct AutoCapture {
    enabled: bool,
    state: State,
    previous: Option<Vec<u8>>,
    captured: Option<Vec<u8>>,
    stable_since: Option<Instant>,
}

impl AutoCapture {
    pub fn new() -> Self {
        Self {
            enabled: true,
            state: State::Baseline,
            previous: None,
            captured: None,
            stable_since: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        if on && !self.enabled {
            self.state = State::Baseline;
            self.captured = None;
            self.stable_since = None;
        }
        self.enabled = on;
    }

    pub fn note_manual_capture(&mut self) {
        if let Some(previous) = &self.previous {
            self.captured = Some(previous.clone());
        }
        self.state = State::Cooldown;
        self.stable_since = None;
    }

    pub fn hint(&self) -> &'static str {
        if !self.enabled {
            return "Auto wyłączone — Spacja robi zdjęcie";
        }
        match self.state {
            State::Baseline => "Połóż stronę pod kamerą",
            State::Armed => "Połóż następną stronę",
            State::Settling => "Trzymaj nieruchomo…",
            State::Cooldown => "Zmień stronę",
        }
    }

    pub fn feed(&mut self, preview: &RgbImage, now: Instant) -> FeedResult {
        let sample = downsample(preview);
        let motion = self
            .previous
            .as_ref()
            .map(|previous| mean_abs_diff(previous, &sample));
        let novelty = self
            .captured
            .as_ref()
            .map(|captured| mean_abs_diff(captured, &sample));
        self.previous = Some(sample.clone());
        if !self.enabled {
            return FeedResult::None;
        }
        match self.state {
            State::Baseline => {
                self.captured = Some(sample);
                self.state = State::Cooldown;
                FeedResult::None
            }
            State::Armed => {
                if novelty.unwrap_or(f32::INFINITY) > NOVELTY_MIN {
                    self.state = State::Settling;
                    self.stable_since = Some(now);
                }
                FeedResult::None
            }
            State::Settling => {
                if novelty.unwrap_or(f32::INFINITY) <= NOVELTY_MIN {
                    self.state = State::Armed;
                    self.stable_since = None;
                    return FeedResult::None;
                }
                if motion.unwrap_or(f32::INFINITY) > MOTION_MAX {
                    self.stable_since = Some(now);
                    return FeedResult::None;
                }
                let stable_since = *self.stable_since.get_or_insert(now);
                if now.duration_since(stable_since) >= Duration::from_millis(STABLE_MS) {
                    self.captured = Some(sample);
                    self.state = State::Cooldown;
                    self.stable_since = None;
                    FeedResult::Trigger
                } else {
                    FeedResult::None
                }
            }
            State::Cooldown => {
                if motion.unwrap_or(0.0) > MOTION_MAX
                    || novelty.unwrap_or(0.0) > NOVELTY_MIN
                {
                    self.state = State::Armed;
                }
                FeedResult::None
            }
        }
    }
}

fn downsample(image: &RgbImage) -> Vec<u8> {
    let small = imageops::thumbnail(image, SAMPLE_WIDTH, SAMPLE_HEIGHT);
    small
        .pixels()
        .map(|pixel| {
            ((pixel[0] as u32 * 54 + pixel[1] as u32 * 183 + pixel[2] as u32 * 19) / 256) as u8
        })
        .collect()
}

fn mean_abs_diff(left: &[u8], right: &[u8]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return f32::INFINITY;
    }
    let total: u64 = left
        .iter()
        .zip(right.iter())
        .map(|(a, b)| a.abs_diff(*b) as u64)
        .sum();
    total as f32 / left.len() as f32
}
```

Tests (same file): uniform frames `frame(level)` = `RgbImage::from_pixel(32, 24, Rgb([level;3]))`; clock `t(base, ms)`. Cover: baseline-no-instant-trigger, place→settle→trigger at ≥700 ms, same-page-no-retrigger, motion-reset during settling, flip→second trigger, disabled-feeds-return-none + re-enable re-baselines, manual capture enters cooldown.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn frame(level: u8) -> RgbImage {
        RgbImage::from_pixel(32, 24, Rgb([level, level, level]))
    }

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn does_not_trigger_on_startup_scene() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        for step in 0..30 {
            assert_eq!(
                auto.feed(&frame(200), t(base, step * 100)),
                FeedResult::None,
                "pusty blat nie może wyzwolić zdjęcia"
            );
        }
    }

    #[test]
    fn triggers_after_page_settles() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0));
        assert_eq!(auto.feed(&frame(100), t(base, 100)), FeedResult::None);
        assert_eq!(auto.feed(&frame(100), t(base, 300)), FeedResult::None);
        assert_eq!(auto.feed(&frame(100), t(base, 900)), FeedResult::Trigger);
        for step in 10..20 {
            assert_eq!(
                auto.feed(&frame(100), t(base, step * 100)),
                FeedResult::None,
                "ta sama strona nie może wyzwolić drugi raz"
            );
        }
    }

    #[test]
    fn motion_resets_the_settle_timer() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0));
        auto.feed(&frame(100), t(base, 100));
        auto.feed(&frame(110), t(base, 500));
        assert_eq!(auto.feed(&frame(100), t(base, 900)), FeedResult::None);
        assert_eq!(auto.feed(&frame(100), t(base, 1300)), FeedResult::Trigger);
    }

    #[test]
    fn flip_produces_second_trigger() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0));
        auto.feed(&frame(100), t(base, 100));
        assert_eq!(auto.feed(&frame(100), t(base, 900)), FeedResult::Trigger);
        auto.feed(&frame(200), t(base, 1000));
        auto.feed(&frame(50), t(base, 1100));
        assert_eq!(auto.feed(&frame(50), t(base, 1200)), FeedResult::None);
        assert_eq!(auto.feed(&frame(50), t(base, 1900)), FeedResult::Trigger);
    }

    #[test]
    fn disabled_never_triggers_and_reenable_rebaselines() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0));
        auto.set_enabled(false);
        auto.feed(&frame(100), t(base, 100));
        assert_eq!(auto.feed(&frame(100), t(base, 900)), FeedResult::None);
        auto.set_enabled(true);
        assert_eq!(auto.feed(&frame(100), t(base, 1000)), FeedResult::None);
        for step in 11..18 {
            assert_eq!(auto.feed(&frame(100), t(base, step * 100)), FeedResult::None);
        }
        auto.feed(&frame(50), t(base, 1900));
        assert_eq!(auto.feed(&frame(50), t(base, 2700)), FeedResult::Trigger);
    }

    #[test]
    fn manual_capture_enters_cooldown() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0));
        auto.feed(&frame(100), t(base, 100));
        auto.note_manual_capture();
        for step in 2..12 {
            assert_eq!(
                auto.feed(&frame(100), t(base, step * 100)),
                FeedResult::None,
                "po ręcznym zdjęciu ta sama strona nie wyzwala auto"
            );
        }
    }
}
```

- [ ] **Step 2: Register `mod autocapture;` in main.rs, SYNC both, TEST**

Expected: 6 new tests pass, everything else still green (dead-code warnings fine until Task 2).

- [ ] **Step 3: Commit** — `feat: auto-capture state machine`

---

### Task 2 (Phase 2): wire auto-capture into ScanHub (+beep, toggle, hint)

**Files:**
- Modify: `Cargo.toml` (windows-sys), `src/app.rs`

**Interfaces:**
- Consumes: Task 1 API; `PipelineEvent`/`capture()` from Phase 1.
- Produces: app fields `autocapture: AutoCapture`, method `beep()`; ScanHub header toggle „Auto: WŁ/WYŁ"; hint label next to camera status.

- [ ] **Step 1: Cargo.toml dependency**

```toml
windows-sys = { version = "0.59", features = ["Win32_UI_WindowsAndMessaging"] }
```

- [ ] **Step 2: app.rs wiring**

Imports: `use crate::autocapture::{AutoCapture, FeedResult};`. Field `autocapture: AutoCapture` (init `AutoCapture::new()`).

`begin_scan` gets `self.autocapture = AutoCapture::new();` (fresh baseline per session).

`capture()` becomes `fn capture(&mut self, manual: bool)`: on successful submit also
```rust
        if manual {
            self.autocapture.note_manual_capture();
        }
        beep();
```
(call sites: Space/button pass `true`; auto path passes `false`). On queue-full, no beep.

Feed point — in `poll_camera`, the `CameraEvent::Preview(image)` arm becomes:

```rust
                CameraEvent::Preview(image) => {
                    self.update_preview_texture(context, &image);
                    self.pending_preview = Some(image);
                }
```
with new field `pending_preview: Option<RgbImage>` (init None). Then in `scan_hub_ui` right after `self.poll_camera(context); self.poll_pipeline(context);`:

```rust
        let dialog_open = self.show_save
            || self.show_cancel_confirm
            || self.show_delete_confirm
            || self.show_settings
            || self.show_new_folder
            || self.show_rename_folder
            || self.show_exit_confirm
            || self.message.is_some();
        if let Some(preview) = self.pending_preview.take() {
            if self.camera_ready && !dialog_open && self.editor.is_none() {
                if self.autocapture.feed(&preview, Instant::now()) == FeedResult::Trigger {
                    self.capture(false);
                }
            }
        }
```
(`self.editor` arrives in Task 5; until then omit that clause — Task 5 adds it.)

Header right side gains toggle + hint (replace the status-label closure):

```rust
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
```
(borrow note: `camera_status`/`camera_ready` are already cloned/copied locals in Phase 1 code, and the closure may borrow `self` mutably — `two_sided`'s closures run sequentially, one `&mut self` capture is fine since the left closure only uses locals; if the borrow checker objects, hoist `let auto_on = self.autocapture.enabled();` and a `let mut toggle_clicked = false;` out of the closure and apply after `two_sided` returns.)

Beep helper at file bottom:

```rust
fn beep() {
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::MessageBeep(0);
    }
}
```

- [ ] **Step 3: SYNC (Cargo.toml + src), TEST, BUILD, Commit** — `feat: auto-capture wired into ScanHub with beep and toggle`

---

### Task 3 (Phase 2): live crop overlay (`overlay.rs`)

**Files:**
- Create: `src/overlay.rs`
- Modify: `src/main.rs` (`mod overlay;`), `src/app.rs`

**Interfaces:**
- Produces: `OverlayDetector::start() -> OverlayDetector`, `submit(&self, frame: RgbImage)` (latest-wins), `latest(&self) -> Option<[CropPoint; 4]>`, `Drop` stops the thread.

- [ ] **Step 1: overlay.rs**

```rust
use crate::document::{CropPoint, detect_document_corners};
use image::RgbImage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const DETECT_INTERVAL_MS: u64 = 330;

pub struct OverlayDetector {
    input: Arc<Mutex<Option<RgbImage>>>,
    output: Arc<Mutex<Option<[CropPoint; 4]>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl OverlayDetector {
    pub fn start() -> Self {
        let input: Arc<Mutex<Option<RgbImage>>> = Arc::new(Mutex::new(None));
        let output: Arc<Mutex<Option<[CropPoint; 4]>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_input = Arc::clone(&input);
        let worker_output = Arc::clone(&output);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                let frame = worker_input.lock().ok().and_then(|mut slot| slot.take());
                if let Some(frame) = frame {
                    let corners = detect_document_corners(&frame);
                    if let Ok(mut slot) = worker_output.lock() {
                        *slot = Some(corners);
                    }
                }
                thread::sleep(Duration::from_millis(DETECT_INTERVAL_MS));
            }
        });
        Self {
            input,
            output,
            stop,
            worker: Some(worker),
        }
    }

    pub fn submit(&self, frame: RgbImage) {
        if let Ok(mut slot) = self.input.lock() {
            *slot = Some(frame);
        }
    }

    pub fn latest(&self) -> Option<[CropPoint; 4]> {
        self.output.lock().ok().and_then(|slot| *slot)
    }
}

impl Drop for OverlayDetector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
```

- [ ] **Step 2: app.rs integration**

Field `overlay: Option<OverlayDetector>` (init None). `begin_scan`: `self.overlay = Some(OverlayDetector::start());`. `abandon_scan`: `self.overlay = None;`.

In the Preview handling added in Task 2, before `pending_preview = Some(image)`:

```rust
                    if let Some(overlay) = &self.overlay {
                        overlay.submit(image.clone());
                    }
```

In `scan_hub_ui`, right after painting the preview texture image (inside the `if let Some(texture) = &self.preview_texture` block, after `painter.image(...)` — the preview draw rect is `Rect::from_center_size(image_bounds.center(), size)`), draw the quad:

```rust
                    if let Some(corners) = self.overlay.as_ref().and_then(OverlayDetector::latest)
                    {
                        let draw_rect = Rect::from_center_size(image_bounds.center(), size);
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
```
(requires binding `let size = …` already present; import `use crate::overlay::OverlayDetector;`.)

- [ ] **Step 3: SYNC, TEST, BUILD, Commit** — `feat: live crop-area overlay on camera preview`

---

### Task 4 (Phase 3): pipeline re-process job

**Files:**
- Modify: `src/pipeline.rs`

**Interfaces:**
- Produces: `submit_reprocess(&self, id: u64, frame: Arc<RgbImage>, corners: [CropPoint; 4]) -> bool`; new events `ReprocessDone { id, page: ScannedPage, corners: [CropPoint; 4] }`, `ReprocessFailed { id, error: String }`.

- [ ] **Step 1: extend Job/PipelineEvent**

```rust
enum Job {
    New { id: u64, frame: Arc<RgbImage> },
    Reprocess { id: u64, frame: Arc<RgbImage>, corners: [CropPoint; 4] },
}
```
`PipelineEvent` gains `ReprocessDone`/`ReprocessFailed` variants as above. `try_submit` builds `Job::New`; new `submit_reprocess` mirrors it. `process_job` matches:

```rust
fn process_job(job: &Job) -> PipelineEvent {
    match job {
        Job::New { id, frame } => { /* existing body, using *id */ }
        Job::Reprocess { id, frame, corners } => match process_page(frame, *corners) {
            Ok(page) => PipelineEvent::ReprocessDone { id: *id, page, corners: *corners },
            Err(error) => PipelineEvent::ReprocessFailed { id: *id, error },
        },
    }
}
```

- [ ] **Step 2: test** — submit_reprocess with explicit corners on the synthetic frame returns `ReprocessDone` with the same corners and A4 output:

```rust
    #[test]
    fn reprocess_uses_caller_corners() {
        let pipeline = ProcessingPipeline::start();
        let corners = [
            CropPoint::new(0.2, 0.2),
            CropPoint::new(0.8, 0.2),
            CropPoint::new(0.8, 0.8),
            CropPoint::new(0.2, 0.8),
        ];
        assert!(pipeline.submit_reprocess(3, Arc::new(white_document_frame(400, 300)), corners));
        let events = collect_events(&pipeline, 1, Duration::from_secs(120));
        match events.first() {
            Some(PipelineEvent::ReprocessDone { id, page, corners: returned }) => {
                assert_eq!(*id, 3);
                assert_eq!((page.width, page.height), (A4_WIDTH_PX, A4_HEIGHT_PX));
                assert_eq!(returned, &corners);
            }
            other => panic!("oczekiwano ReprocessDone, było: {other:?}"),
        }
    }
```
(add `#[derive(Debug)]`-friendly formatting by implementing a manual match — `PipelineEvent` has no Debug; in the panic arm print a static label instead: `panic!("oczekiwano ReprocessDone")`.)

- [ ] **Step 3: SYNC, TEST (new test passes, old ones green), Commit** — `feat: pipeline re-process job with caller corners`

---

### Task 5 (Phase 3): PageEditor

**Files:**
- Modify: `src/app.rs`

**Interfaces:**
- Produces: field `editor: Option<EditorState>`; `PageSlot::Reprocessing { original_jpeg: Vec<u8> }`; selection-row button „Popraw kadr"; editor screen drawn INSTEAD of ScanHub content while open (camera keeps polling).

- [ ] **Step 1: state + open/close**

```rust
struct EditorState {
    slot_index: usize,
    original: RgbImage,
    texture: TextureHandle,
    corners: [CropPoint; 4],
}
```
Field `editor: Option<EditorState>` (init None). `PageSlot` gains `Reprocessing { original_jpeg: Vec<u8> }`. `can_save` already excludes it (not `Ready`). Open (called from selection row):

```rust
    fn open_editor(&mut self, index: usize, context: &egui::Context) {
        let Some(entry) = self.slots.get(index) else { return };
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
            Err(error) => {
                self.message = Some(format!("Nie można odczytać oryginału: {error}"))
            }
        }
    }
```
with

```rust
fn fallback_editor_corners() -> [CropPoint; 4] {
    [
        CropPoint::new(0.06, 0.06),
        CropPoint::new(0.94, 0.06),
        CropPoint::new(0.94, 0.94),
        CropPoint::new(0.06, 0.94),
    ]
}
```

Apply:

```rust
    fn apply_editor(&mut self) {
        let Some(editor) = self.editor.take() else { return };
        let Some(entry) = self.slots.get_mut(editor.slot_index) else { return };
        let Some(pipeline) = &self.pipeline else { return };
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
```
(import `std::sync::Arc` in app.rs.)

`poll_pipeline` gains arms:

```rust
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
```

Film strip: `Reprocessing` renders like `Processing` (spinner tile).

- [ ] **Step 2: editor UI**

In `scan_hub_ui`, at the very top after polls: if `self.editor.is_some()`, draw the editor INSTEAD of the hub body and return:

```rust
        if self.editor.is_some() {
            self.editor_ui(ui, context);
            return;
        }
```

```rust
    fn editor_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        let Some(editor) = &mut self.editor else { return };
        let mut close = false;
        let mut redetect = false;
        let mut apply = false;
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
                            Button::new(
                                RichText::new("Zastosuj").strong().color(Color32::WHITE),
                            )
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
```
plus

```rust
fn constrain_editor_corner(index: usize, point: CropPoint) -> CropPoint {
    match index {
        0 => CropPoint::new(point.x.min(0.49), point.y.min(0.49)),
        1 => CropPoint::new(point.x.max(0.51), point.y.min(0.49)),
        2 => CropPoint::new(point.x.max(0.51), point.y.max(0.51)),
        3 => CropPoint::new(point.x.min(0.49), point.y.max(0.51)),
        _ => point,
    }
}
```
(borrow note: `editor_ui` first borrows `self.editor` mutably inside the closure while calling `page_container(ui, |ui| …)` which doesn't touch self — the closure captures `editor` (from the outer `let Some(editor) = &mut self.editor`), and `close/redetect/apply` are locals; `self.apply_editor()` runs after the borrow ends.)

Selection row (Phase 1 code) gains, before „Usuń stronę":

```rust
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
```

Auto-capture gate from Task 2 adds `&& self.editor.is_none()`. Keyboard gate in `ui()` adds `|| self.editor.is_some()` hmm — Space in editor must NOT capture: extend `dialog_open` computation in `ui()` with `|| self.editor.is_some()`.

- [ ] **Step 3: SYNC, TEST, BUILD, Commit** — `feat: page editor with re-crop on original frame`

---

### Task 6 (Phase 4): `session.rs` store

**Files:**
- Create: `src/session.rs`
- Modify: `src/main.rs` (`mod session;`), `src/document.rs` (add `page_from_jpeg_bytes`)

**Interfaces:**
- Produces:
  - `SessionStore::open_default() -> Option<SessionStore>` (ProjectDirs data dir + `sesja`)
  - `SessionStore::at(dir: PathBuf) -> SessionStore` (tests)
  - `begin(&self, folder: &Path) -> Result<(), String>` (wipe + manifest with empty page list)
  - `write_page(&self, id: u64, jpeg: &[u8]) -> Result<(), String>`
  - `remove_page(&self, id: u64) -> Result<(), String>`
  - `set_order(&self, ids: &[u64]) -> Result<(), String>` (rewrite manifest)
  - `load_existing(&self) -> Option<RecoveredSession>` where `struct RecoveredSession { pub folder_path: PathBuf, pub pages: Vec<(u64, Vec<u8>)> }`
  - `clear(&self) -> Result<(), String>`
- `document::page_from_jpeg_bytes(jpeg: Vec<u8>) -> Result<ScannedPage, String>`

- [ ] **Step 1: document.rs helper**

```rust
pub fn page_from_jpeg_bytes(jpeg: Vec<u8>) -> Result<ScannedPage, String> {
    let image = decode_jpeg(&jpeg)?;
    let review_image = resize_to_fit(&image, 1200, 1200, imageops::FilterType::Lanczos3);
    Ok(ScannedPage {
        width: image.width(),
        height: image.height(),
        jpeg,
        review_image,
    })
}
```

- [ ] **Step 2: session.rs**

```rust
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct Manifest {
    folder_path: PathBuf,
    started_at: u64,
    page_ids: Vec<u64>,
}

pub struct RecoveredSession {
    pub folder_path: PathBuf,
    pub pages: Vec<(u64, Vec<u8>)>,
}

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn open_default() -> Option<Self> {
        ProjectDirs::from("pl", "SkanerDokumentow", "Skaner dokumentów")
            .map(|dirs| Self { dir: dirs.data_dir().join("sesja") })
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.ron")
    }

    fn page_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.jpg"))
    }

    fn read_manifest(&self) -> Option<Manifest> {
        let contents = fs::read_to_string(self.manifest_path()).ok()?;
        ron::from_str(&contents).ok()
    }

    fn write_manifest(&self, manifest: &Manifest) -> Result<(), String> {
        let contents = ron::to_string(manifest).map_err(|error| error.to_string())?;
        fs::write(self.manifest_path(), contents).map_err(io_error)
    }

    pub fn begin(&self, folder: &Path) -> Result<(), String> {
        self.clear()?;
        fs::create_dir_all(&self.dir).map_err(io_error)?;
        self.write_manifest(&Manifest {
            folder_path: folder.to_path_buf(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
            page_ids: Vec::new(),
        })
    }

    pub fn write_page(&self, id: u64, jpeg: &[u8]) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        fs::write(self.page_path(id), jpeg).map_err(io_error)?;
        if !manifest.page_ids.contains(&id) {
            manifest.page_ids.push(id);
        }
        self.write_manifest(&manifest)
    }

    pub fn remove_page(&self, id: u64) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        let _ = fs::remove_file(self.page_path(id));
        manifest.page_ids.retain(|existing| *existing != id);
        self.write_manifest(&manifest)
    }

    pub fn set_order(&self, ids: &[u64]) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        manifest.page_ids = ids.to_vec();
        self.write_manifest(&manifest)
    }

    pub fn load_existing(&self) -> Option<RecoveredSession> {
        let manifest = self.read_manifest()?;
        if manifest.page_ids.is_empty() {
            return None;
        }
        let mut pages = Vec::new();
        for id in &manifest.page_ids {
            let bytes = fs::read(self.page_path(*id)).ok()?;
            pages.push((*id, bytes));
        }
        Some(RecoveredSession {
            folder_path: manifest.folder_path,
            pages,
        })
    }

    pub fn clear(&self) -> Result<(), String> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir).map_err(io_error)?;
        }
        Ok(())
    }
}

fn io_error(error: std::io::Error) -> String {
    format!("Błąd zapisu sesji: {error}")
}
```

Tests: temp dir (`std::env::temp_dir().join(format!("skaner-sesja-test-{}", std::process::id()))`): begin → load_existing None (empty ids); write two pages (ids 5, 9) → load returns both in order with exact bytes and folder path; remove_page(5) → only 9; set_order after re-adding → order respected; clear → load None; ALSO tolerance test: write_page on missing manifest errors (no panic). Always `clear()` at test end.

- [ ] **Step 3: SYNC (session.rs, document.rs, main.rs), TEST, Commit** — `feat: session store for crash recovery`

---

### Task 7 (Phase 4): session integration + restore dialog

**Files:**
- Modify: `src/app.rs`

**Interfaces:**
- Produces: fields `session: Option<SessionStore>`, `session_broken: bool`, `recovered: Option<RecoveredSession>`, `show_restore: bool`.

- [ ] **Step 1: hooks**

Imports: `use crate::session::{RecoveredSession, SessionStore};` and `use crate::document::page_from_jpeg_bytes;`.

In `new()` (after `restore_last_folder()`):

```rust
        app.session = SessionStore::open_default();
        if let Some(recovered) = app
            .session
            .as_ref()
            .and_then(SessionStore::load_existing)
        {
            app.recovered = Some(recovered);
            app.show_restore = true;
        }
```

`begin_scan`: after starting pipeline —

```rust
        self.session_broken = false;
        if let (Some(session), Some(folder)) = (&self.session, &self.selected_folder)
            && let Err(error) = session.begin(&folder.path)
        {
            self.session_broken = true;
            self.toast = Some(Toast {
                text: format!("Kopia sesji wyłączona: {error}"),
                shown_at: Instant::now(),
            });
        }
```

Add a small helper used by every mutation site:

```rust
    fn session_write_page(&mut self, id: u64, jpeg: &[u8]) {
        if self.session_broken {
            return;
        }
        if let Some(session) = &self.session
            && let Err(error) = session.write_page(id, jpeg)
        {
            self.session_broken = true;
            self.toast = Some(Toast {
                text: format!("Kopia sesji wyłączona: {error}"),
                shown_at: Instant::now(),
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
        }
        let _ = error_unused(); // (see note below)
    }
```
NOTE: the stray `error_unused` line above is a planning artifact — implement `session_sync_order` exactly like `session_write_page` (match on the error, set `session_broken`, show toast). Do not add helper stubs.

Call sites:
- `poll_pipeline` `PageReady`/`ReprocessDone`: after slot update → `self.session_write_page(id, &<page jpeg bytes>)` (capture `page.jpeg.clone()`? No — call BEFORE moving `page` into `PageData`: hold `let jpeg_copy_needless = ();` — implement by calling `self.session_write_page(id, &page.jpeg)` first, then build the texture/PageData from `page`).
- `rotate_selected_page` success → `self.session_write_page(entry.id, &data.page.jpeg)` (after replacing `data.page`; note entry.id must be read before the `&mut` dance — bind `let entry_id = entry.id;` at the top).
- `delete_selected_page` → before removing, read `let removed_id = self.slots[index].id;`; after removing → `if let Some(session) = &self.session { let _ = session.remove_page(removed_id); }` then `self.session_sync_order();`.
- `move_selected_page` → `self.session_sync_order();` after the swap.
- `save_current_document` Ok arm and `abandon_scan` → `if let Some(session) = &self.session { let _ = session.clear(); }`.

- [ ] **Step 2: restore dialog + restore action**

In `dialogs()`:

```rust
        if self.show_restore {
            egui::Window::new("Niezapisana sesja")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    let (count, folder_display, folder_exists) = match &self.recovered {
                        Some(recovered) => (
                            recovered.pages.len(),
                            recovered.folder_path.display().to_string(),
                            recovered.folder_path.is_dir(),
                        ),
                        None => (0, String::new(), false),
                    };
                    ui.label(format!(
                        "Znaleziono niezapisaną sesję ({}).",
                        polish_page_count(count)
                    ));
                    ui.label(format!("Folder: {folder_display}"));
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
```

```rust
    fn restore_recovered_session(&mut self, context: &egui::Context) {
        let Some(recovered) = self.recovered.take() else { return };
        let folder_name = recovered
            .folder_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.selected_folder = Some(FolderInfo {
            name: folder_name,
            path: recovered.folder_path.clone(),
            pdf_count: 0,
        });
        self.slots.clear();
        self.selected_slot = None;
        self.pending_jobs = 0;
        self.filename.clear();
        self.pipeline = Some(ProcessingPipeline::start());
        self.overlay = Some(OverlayDetector::start());
        self.autocapture = AutoCapture::new();
        self.session_broken = false;
        let mut max_id = 0;
        for (id, jpeg) in recovered.pages {
            max_id = max_id.max(id);
            match page_from_jpeg_bytes(jpeg) {
                Ok(page) => {
                    let texture = context.load_texture(
                        format!("strona-{id}"),
                        rgb_to_color_image(&page.review_image),
                        TextureOptions::LINEAR,
                    );
                    self.slots.push(SlotEntry {
                        id,
                        slot: PageSlot::Ready(Box::new(PageData {
                            page,
                            original_jpeg: Vec::new(),
                            corners: fallback_editor_corners(),
                            texture,
                        })),
                    });
                }
                Err(error) => self.message = Some(error),
            }
        }
        self.next_page_id = max_id + 1;
        self.refresh_pdfs();
        self.start_camera();
    }
```
(Restored pages keep the existing on-disk session files — do NOT call `session.begin` here, the manifest already matches; `session_broken` stays false so further pages append.)
Wait — `begin_scan` is NOT called on restore, so the session dir survives; new captures call `session_write_page` which appends to the existing manifest. Correct as written.

`restore_last_folder` conflict: restore dialog may appear while a folder is already open — fine, dialog sits on top; „Przywróć" replaces the open folder.

- [ ] **Step 3: SYNC, TEST, BUILD, Commit** — `feat: crash-recovery session with restore dialog`

---

### Task 8: Full verification pass

- [ ] **Step 1:** `cargo fmt` (commit if diff), full TEST (expect: pipeline 3, autocapture 6, session ~5, document 5+1, storage 1 — all ok), BUILD `Finished`.
- [ ] **Step 2:** Launch on gm desktop (schtasks pattern). WITHOUT a camera the expected behavior: Library/Folder browsing works, restore dialog absent (no session), „Nowy skan" shows camera-error status with „Spróbuj ponownie" — pages/film strip/save inactive. No crash after 60 s idle in ScanHub.
- [ ] **Step 3:** Kill-test recovery without camera is impossible (no pages) — recovery smoke deferred to the production machine with the device (documented in final report).
- [ ] **Step 4:** Commit any fixes; update memory + final report for tomorrow's production install.

## Self-Review Notes

- Spec §5 mapped: states/thresholds/hints in Task 1 (Baseline deviation documented); §4 overlay in Task 3; §3 editor in Task 5 (button-open deviation documented); §6 recovery in Tasks 6–7 (UI-thread writes deviation documented); beep §3 in Task 2.
- Type consistency: `PipelineEvent::{ReprocessDone,ReprocessFailed}` names match between Tasks 4 and 5; `SessionStore` API names match between Tasks 6 and 7; `fallback_editor_corners` defined in Task 5, used in Task 7.
- Placeholder scan: the one intentional annotation (`error_unused` in Task 7 Step 1) is explicitly flagged with implementation instructions — no other TBDs.
