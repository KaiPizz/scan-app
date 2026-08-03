# Conveyor Scan Workflow — Design Spec

**Date:** 2026-08-03 · **Author:** Paul (KaiPizz) + Claude · **Status:** Approved by owner

## 1. Context and Goals

The app (`skaner-dokumentow`, Rust + eframe/egui + nokhwa, IRIScan Visualizer 7 camera)
currently forces a per-page loop of `Scan → Crop (confirm) → Review → "Dodaj stronę" → Scan`.
Owner scans **~100 pages/day**, mixed between many small documents (1–5 pages each)
and a few thick stacks (tens of pages).

Audit findings (v0.2.1, commit `b375576`):

1. **Camera is stopped after every capture** (`capture()` → `stop_camera()`) and fully
   restarted on "Dodaj stronę": MediaFoundation re-query (up to 8×650 ms), device opened
   twice (format probe + real open). ≈ 2–4 s dead time per page.
2. **3 clicks + 2 screen transitions per page** even when corner auto-detect is correct.
3. **Image processing runs synchronously on the UI thread** (warp 13 MP → A4 + contrast
   stretch + JPEG q91 + Lanczos3 review resize ≈ 1–1.5 s UI freeze per page).
   `crop_busy` is set and cleared within one frame — dead code.
4. **No keyboard shortcuts.**
5. **Data-loss bug:** "Anuluj dokument" in Review wipes all scanned pages with **no
   confirmation** (single-page delete does confirm).
6. Keep as-is (works well): corner detection (gradient scoring + Hough refinement),
   atomic PDF write (`.part` → rename), unique-name generation, Windows filename
   validation, A4 300 dpi output quality.

**Goal:** page throughput limited only by how fast the operator flips paper.
Zero mandatory clicks per page. UI never blocks. No lost work on crash.

## 2. Non-Goals

- No OCR, no cloud upload, no multi-camera support.
- No change to Library/Folder screens, storage naming rules, PDF writer, or camera
  discovery/open logic.
- No foot-pedal integration (Space already covers external HID triggers if ever needed).
- UI language stays Polish.

## 3. UX Design

### Screens

`Library` and `Folder` unchanged. `Scan`, `Crop`, `Review` are **replaced** by:

- **ScanHub** — the single scanning screen.
- **PageEditor** — opened by clicking a thumbnail in ScanHub.

### ScanHub

```
┌─ top bar: „Skanowanie · N stron” · camera status · [Auto: WŁ/WYŁ] ─┐
│  live preview (camera never stops)                                 │
│  + blue quad overlay = live-detected crop area (~3 Hz)             │
│  + status hint: „Połóż stronę” / „Trzymaj nieruchomo…” / „Zrobione”│
│  [Zrób zdjęcie (Spacja)]   [Zapisz dokument (Enter)]   [Anuluj]    │
│  film strip: [1][2][3][⌛4]…  ← click = PageEditor                  │
└────────────────────────────────────────────────────────────────────┘
```

- Camera starts on entry and stays running until the user leaves ScanHub
  (save does **not** stop it; cancel/back does).
- **Auto-capture ON by default**, toggle in top bar. Manual capture (button or
  **Space**) always works, regardless of the toggle.
- Captured frame goes to a background pipeline; a placeholder thumbnail (⌛)
  appears instantly and is replaced by the processed page thumbnail.
- Beep (`MessageBeep`) on every capture (auto and manual).

### Auto-capture behavior (operator's view)

Place page → app waits for it to settle (~0.7 s stillness) → beep + capture →
app waits until the scene changes (page lifted/flipped) → repeat. A one-line
status hint always says what the machine is waiting for.

### PageEditor

Opened per page from the film strip. Contains: large processed-page view, 4-corner
crop editing over the **original captured frame**, rotate 90° CW, delete (confirm),
move left/right, re-detect corners, „Zastosuj" (re-process in background), back.
While the editor is open, **auto-capture is paused** (camera keeps running);
returning to ScanHub resumes it.

### Save flow

**Enter** (or button) → dialog with focused filename input + page count →
type name → **Enter** saves (Esc cancels). Extension `.pdf` auto-appended,
unique-name logic unchanged. On success: non-modal toast „Zapisano: <name>.pdf”
(auto-hide ~4 s), pages cleared, **camera still running** — next document starts
immediately. The old modal "Dokument zapisany" is removed.

Save is **disabled while the pipeline queue is non-empty**, label shows
„Przetwarzanie X stron…” (drains in seconds).

### Fixed bugs / confirmations

- „Anuluj dokument” with ≥1 page → confirmation dialog (new).
- Window close with unsaved pages → existing confirmation (kept).
- Page delete keeps its confirmation.

### Keyboard map

| Key | Context | Action |
|---|---|---|
| Space | ScanHub | Manual capture |
| Enter | ScanHub, ≥1 page, queue empty | Open save dialog |
| Enter | Save dialog | Save |
| Esc | Save dialog / PageEditor | Close / back |

## 4. Architecture

| Module | Role |
|---|---|
| `camera.rs` | **Unchanged.** Used differently: one `CameraController` lives for the whole ScanHub session. |
| `autocapture.rs` **new** | Pure state machine. Input: preview frames + timestamps. Output: capture triggers + UI hint state. No camera/UI dependency → unit-testable with synthetic frames. |
| `pipeline.rs` **new** | One worker thread. Job: original full-res frame → detect corners → `process_page` (warp/enhance/encode) → emit `PageReady`. Also encodes original JPEG and writes session-recovery files. FIFO → page order preserved. |
| `session.rs` **new** | Session-recovery directory: write/load/discard manifest + per-page files. |
| `document.rs` | Unchanged (existing `detect_document_corners`, `process_page`, `rotate_page_clockwise`, `save_pdf`). |
| `storage.rs` | Unchanged. |
| `app.rs` | ScanHub + PageEditor UI; owns pipeline + autocapture instances; screen enum becomes `Library, Folder, ScanHub, PageEditor`. |

### Data flow

```
camera thread ── Preview(1280×960) ──► app ── feed ──► autocapture ── Trigger ──► capture
                                        │                                            │
                                        └── overlay quad (detect @ ~3 Hz) ◄──────────┤
capture: latest_full_image() ── Arc<RgbImage> ──► pipeline worker                    │
   pipeline: detect corners → process_page → encode original JPEG                    │
             → write session files → PageReady{slot, page, original_jpeg} ──► app UI ┘
```

### Page data model

```rust
enum PageSlot {
    Processing,                          // placeholder thumbnail
    Ready(PageData),
    Failed { original_jpeg: Vec<u8>, error: String },  // ⚠ thumbnail, fixable in editor
}
struct PageData {
    page: ScannedPage,        // processed JPEG + review image (existing struct)
    original_jpeg: Vec<u8>,   // q≈88 full-frame JPEG for later re-crop (~2–3 MB)
    corners: [CropPoint; 4],  // corners used for the current processing
}
```

Memory budget: ~5 MB/page (processed + original JPEG + review bitmap) → ~500 MB at
100 pages in one document; typical stacks (≤60) well under that. Pipeline queue holds
raw frames (~37 MB each) — **queue capacity 8**; when full, capture is refused with a
status hint (practically unreachable: flipping ≈ 2–3 s/page > processing ≈ 1–1.5 s/page).

### Live overlay

`detect_document_corners` on the 1280×960 preview (function already downscales to 720)
every ~330 ms. Cost ≈ 10–30 ms — never on the UI thread: a helper thread consumes the
latest preview and updates a shared `Option<[CropPoint;4]>` that the UI just draws.

New dependency: `windows-sys` (MessageBeep). Everything else is std + existing crates.

## 5. Auto-capture State Machine (`autocapture.rs`)

Input each preview frame: downscale to gray ~160 px wide. Metrics:

- `motion` = mean abs diff vs previous frame (per-pixel, 0–255 scale).
- `novelty` = mean abs diff vs fingerprint of the **last captured** frame.

States and transitions (tunable constants in one block):

```
Idle          — auto OFF or editor open. → Armed when enabled.
Armed         — waiting for a new page: needs novelty > NOVELTY_MIN (≈ 12)
                (scene differs from last capture) → Settling.
Settling      — needs motion < MOTION_MAX (≈ 2.5) continuously for STABLE_MS (≈ 700).
                Any motion spike resets the timer. → Trigger.
Trigger       — emit capture, store fingerprint of captured frame → Cooldown.
Cooldown      — wait for the operator to remove/flip the page:
                needs motion > MOTION_MAX for a moment OR novelty > NOVELTY_MIN
                vs the just-captured fingerprint → Armed.
```

First page after entering ScanHub: fingerprint = empty ⇒ `novelty` trivially high ⇒
flows through Settling like any page. UI hints map 1:1 to states
(Armed → „Połóż stronę”, Settling → „Trzymaj nieruchomo…”, Cooldown → „Zmień stronę”).
Manual capture in any state jumps to Trigger's bookkeeping (fingerprint + Cooldown).

Thresholds validated by unit tests with synthetic sequences; expose them as consts
so field tuning is a one-line change.

## 6. Session Recovery (`session.rs`)

- Location: `ProjectDirs("pl","SkanerDokumentow","Skaner dokumentów").data_dir()/sesja/`.
  Single active session (no timestamped multiplicity — one app instance).
- Files: `manifest.ron` `{ folder_path, started_at, pages: [page_file, …] }` +
  `NNN.jpg` = **processed** page JPEG (bytes already produced by pipeline).
  Originals are NOT persisted (recovery is an emergency path; recovered pages can be
  rotated/deleted/reordered but not re-cropped).
- Lifecycle: pipeline worker appends page file + rewrites manifest after each
  `PageReady`; editor operations (delete/rotate/reorder/re-crop) rewrite affected
  files in the background; successful save or explicit cancel → directory removed.
- Startup: if manifest exists with ≥1 page → dialog „Znaleziono niezapisaną sesję
  (N stron) w folderze <X>. Przywrócić?” → Restore (loads pages into ScanHub for that
  folder) / Usuń (delete dir). If the target folder no longer exists, Restore first asks
  the user to pick an existing folder (Library screen), then loads the pages there.
- Disk-write failure (e.g. disk full): one warning toast, recovery disabled for the
  session, scanning continues unaffected.

## 7. Error Handling

| Failure | Behavior |
|---|---|
| Camera error / unplug mid-session | Status bar red + autocapture → Idle; pages intact; „Spróbuj ponownie” restarts camera only. |
| `process_page` fails (degenerate corners) | Slot → `Failed` with ⚠ thumbnail; PageEditor opens with fallback corners on the original; „Zastosuj” retries. |
| Pipeline queue full | Capture refused + hint „Poczekaj — przetwarzanie…” (auto-capture waits, does not drop). |
| Save while queue non-empty | Save button disabled with progress label (see §3). |
| Beep API failure | Ignored (best-effort). |

## 8. Performance Budgets

- Capture-to-ready-for-next-capture: **0 ms** blocking (camera never stops; frame grab
  is an `Arc` clone).
- UI thread: no single frame > 16 ms attributable to scanning logic (all heavy work on
  worker threads; overlay detect on helper thread).
- Page processing latency (background): ≤ 2 s/page on gm-class hardware.
- Auto-capture reaction (page settles → beep): ~ `STABLE_MS` + ≤ 2 preview frames.

## 9. Testing Plan

- `autocapture.rs`: unit tests — synthetic gray-frame sequences covering: settle→trigger,
  motion-reset during Settling, cooldown until page flip, same-page-no-retrigger,
  first-page trigger, toggle off/on, editor pause.
- `pipeline.rs`: submit synthetic image → receive ordered `PageReady`; failure path
  (degenerate corners) → `Failed` slot; queue-cap behavior.
- `session.rs`: write→load roundtrip, manifest rewrite on delete/reorder, discard.
- `document.rs`/`storage.rs`: existing tests keep passing (no changes expected).
- Manual smoke with the real IRIScan (existing `#[ignore]` tests + hands-on run):
  30-page stack, 3 small docs in a row, unplug mid-session, crash-kill + restore.

## 10. Implementation Phases

1. **Keep-alive capture core** — camera survives across captures; Space; film strip in
   ScanHub; pipeline worker with placeholder thumbnails; save dialog rework + toast;
   cancel-confirm bug fix. (App is already dramatically faster here.)
2. **Auto-capture** — `autocapture.rs` + overlay + hints + beep + toggle.
3. **PageEditor** — crop-on-original, rotate/delete/reorder/re-detect/re-apply.
4. **Session recovery** — `session.rs` + startup restore dialog.

Each phase lands compiling + tested; order chosen so the app is usable after every phase.

## 11. Decisions Log (owner Q&A)

- Daily volume is a **mix** of small docs and thick stacks → save flow must loop fast.
- Trigger: **auto-capture** default ON, manual Space always available.
- Crop: **trust auto-detect 100%**, never block the conveyor; fix via thumbnail later.
- Naming: **typed at save time** (focused input, Enter), no auto-naming.
- Approach: **C** — conveyor + crash recovery (over minimal de-friction A and
  conveyor-only B).
