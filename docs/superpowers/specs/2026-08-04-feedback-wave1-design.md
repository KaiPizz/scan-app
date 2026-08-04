# Feedback Wave 1 — Quality, Detection, Document Lifecycle: Design Spec

**Date:** 2026-08-04 · **Author:** Paul (KaiPizz) + Claude · **Status:** Approved by owner

## 1. Context

First real-user test (morning 2026-08-04, klaud-laptop + IRIScan Visualizer 7, sample:
`Desktop\A02\A02 (2).pdf`) surfaced six issues. Forensics on the sample PDF and code:

| # | Symptom | Root cause (verified) |
|---|---------|----------------------|
| 1 | Scans blurry despite "4K" camera | Embedded images are **703×994 px (85 DPI)** while the pipeline produces 2480×3508 (300 DPI). 703/2480 = 85/300 exactly → `save_pdf` lets `printpdf` `PdfSaveOptions::default()` **recompress + downsample** every page. Camera itself is 13MP Sony, up to 4160×3120, AF-C/AF-S (vendor specs). |
| 2 | Whole documents tinted blue/gray, tint varies per document | Camera AWB drifts; `enhance_document` stretches contrast using a **luminance** histogram with one common scale, preserving any color cast. |
| 3 | Auto-crop bites into tables instead of page edges | `detect_document_corners` picks the **strongest Hough line** within wide search bands (up to 45% into the page); table rules beat paper edges. |
| 4 | No way to open the PDF right after saving | UI wave replaced the old "saved" modal (which had „Otwórz PDF") with a plain toast. Regression. |
| 5 | A bad page or bad document forces a full rescan; PDFs cannot be deleted | No delete/edit for saved PDFs; no pre-save review. |
| 6 | Auto-capture feels random; sometimes fires with an **empty mat** | No document-present gate (novelty/motion alone can fire on shadows/hands/light); no visual progress toward the trigger. |

## 2. Non-Goals

- No OCR, no cloud, no multi-camera.
- No UVC camera-control panel (focus/AWB sliders) in this wave — a separate probe
  will measure the negotiated resolution and raw sharpness first (§3.3); manual UVC
  controls only if that probe proves the camera needs them.
- No editing of PDFs produced by other software (only this app's own output format).

## 3. Cluster 1 — Image Quality

### 3.1 Stop the PDF downsampling (issue 1)

`save_pdf` must embed each page's existing 300-DPI JPEG **byte-for-byte** (or at
minimum with optimization disabled). Concretely: construct `PdfSaveOptions` with
image optimization off instead of `::default()` (exact field per printpdf 0.12.5
API, e.g. `image_optimization: None` / `optimize: false` — pin at implementation
after checking the crate docs). Acceptance: `pdfimages -list` on a fresh scan shows
**2480×3508** at ~300 x-ppi/y-ppi.

### 3.2 Neutral background normalization (issue 2)

Replace the luminance-based stretch in `enhance_document` with **per-channel**
normalization: for each of R, G, B compute low = 0.5th percentile, high = 99.5th
percentile (sampled, as today), then map that channel `low→5, high→250` (clamped).
White paper maps to neutral white regardless of cast; skip the channel (or the whole
adjustment) when `high − low < 60` as today. Unit test: synthetic blue-tinted "paper"
(e.g. RGB 190,200,230 background, dark text) comes out with background channels within
±6 of each other and ≥240.

### 3.3 Camera probe (diagnostic, not app code)

When the laptop app is idle (evening or coordinated via UltraViewer): run the ignored
camera test with `IRISCAN_TEST_FRAME` to record the negotiated format + a raw frame.
If MJPEG tops out below 4160×3120, extend `preferred_formats` to admit NV12/YUY2 at
higher resolutions (follow-up decision with the frame evidence in hand).

## 4. Cluster 2 — Detection & Trustworthy Auto-Capture

### 4.1 Contour-first corner detection (issue 3)

Rewrite `detect_document_corners` as a hybrid:

1. **Bright-region contour (primary):** downscale to ~720 px, adaptive threshold
   (paper is bright on the black mat), largest connected bright component, take its
   outer contour; reject if area < 18% of frame or touching < allowed margins.
   Corner estimate = min-area quadrilateral of the contour (or extreme-point fit).
2. **Edge refinement (secondary):** for each contour edge, run line fitting (Hough or
   least-squares on Canny points) **only within a ±3% band around that contour edge**.
   Table lines live deep inside the contour and can no longer win.
3. **Confidence:** `DetectResult { corners, confident: bool }` — confident requires a
   valid quad (area, side lengths, in-bounds as today) AND inside/outside brightness
   contrast above a threshold. Fallback corners ⇒ `confident = false`.

Existing signature callers adapt (`detect_document_corners` returns the struct; the
pipeline and editor use `.corners`). Unit tests: existing synthetic tests keep passing
(adapted), plus a new test with a synthetic page containing a dark inner table grid —
detected corners must stay on the outer paper boundary (tolerance 2%).

### 4.2 No-document, no-trigger (issue 6a)

`AutoCapture::feed` gains a `document_present: bool` input (from the overlay
detector's latest confident result, sampled at feed time). `Settling → Trigger`
requires `document_present`; without it the state falls back to Armed. Empty mat,
hands, and lighting changes can no longer fire the shutter. Unit tests extend the
synthetic suite with a `document_present=false` sequence that must never trigger.

### 4.3 Capture progress ring (issue 6b)

The live overlay quad becomes a progress indicator, Genius-Scan style: while
Settling, the quad's border is drawn as an animated sweep — a bright segment growing
clockwise from the top-left corner, covering `settle_progress` (0→1 over `STABLE_MS`,
exposed by `AutoCapture::settle_progress(now)`). At 100% the frame is captured and the
full border flashes green ~300 ms. Armed/Cooldown draw the normal thin blue quad;
no document → no quad. The status hint line stays as-is.

## 5. Cluster 3 — Document Lifecycle

### 5.1 Toast with „Otwórz PDF" (issue 4)

The save toast keeps the saved path, shows for **8 s**, and contains an inline
„Otwórz PDF" button (opens via `open::that_detached`, same error handling as the
folder list). Clicking does not clear the conveyor; scanning continues.

### 5.2 Pre-save review screen (issue 5a)

„Zapisz dokument" (button or Enter) now opens a **Przegląd** screen instead of the
bare dialog: large preview of the current page, ←/→ navigation (buttons + arrow
keys), the film strip stays visible and clickable, per-page actions reuse the
selection-row set (rotate / re-crop via PageEditor / delete / reorder), and a
always-visible filename field with „Zapisz PDF (Enter)" + „Wróć (Esc)". Typing a
name and pressing Enter saves immediately from any page — reviewing every page is
possible, never mandatory. The old save dialog is removed; save gating rules
(all pages Ready) unchanged.

### 5.3 Delete and edit saved PDFs (issue 5b)

Folder list rows gain „Usuń" (confirmation dialog; `fs::remove_file`) and „Edytuj".

**Edit** re-opens one of the app's own PDFs into the conveyor:
- Extraction: scan the PDF bytes for embedded JPEG streams (DCTDecode objects —
  every page this app writes is exactly one full-page JPEG XObject; with §3.1 the
  bytes are the originals). Implemented in `document.rs` as
  `pub fn extract_pdf_pages(path: &Path) -> Result<Vec<Vec<u8>>, String>`; each JPEG
  rebuilds a `ScannedPage` via the existing `page_from_jpeg_bytes`.
- The pages load into ScanHub slots (camera starts, session recovery active, marked
  like recovered pages: no original frame ⇒ re-crop unavailable, rotate/delete/append
  fine), the filename field pre-fills the existing name, and saving **overwrites the
  source file atomically** (write `.part`, rename over) instead of uniquifying.
  A `editing_target: Option<PathBuf>` field drives the overwrite; „Anuluj dokument"
  leaves the original untouched.
- PDFs that yield zero JPEG streams (foreign files dropped into the folder) show
  „Ten PDF nie pochodzi z tego programu" and open nothing.

## 6. Error Handling Additions

| Case | Behavior |
|---|---|
| Extraction fails / foreign PDF | Message dialog, folder list unchanged. |
| Overwrite fails mid-save (edit mode) | `.part` file removed, original intact, error dialog (existing atomic pattern). |
| Delete PDF fails (file locked by viewer) | Error dialog with the OS message. |
| Detector not confident | No auto-capture, no progress ring; manual Space still captures with fallback corners. |

## 7. Testing

- `document.rs`: per-channel normalization test (§3.2); table-grid corner test (§4.1);
  `extract_pdf_pages` round-trip test (save 2 synthetic pages → extract → 2 JPEGs that
  decode to the right dimensions); existing suite adapted to `DetectResult`.
- `autocapture.rs`: `document_present` gating tests; `settle_progress` monotonicity test.
- Manual (with device): table-heavy page crops to paper edges; empty mat never fires;
  ring completes → beep; edit flow on a real stack; `pdfimages -list` shows 300 DPI.

## 8. Delivery Order

1. **Cluster 1** (§3.1 + §3.2) — smallest diff, biggest impact; ship to the laptop same-day.
2. **Cluster 2** (§4) — detector + gate + ring.
3. **Cluster 3** (§5) — toast button, review screen, delete/edit PDFs.

Each cluster lands green (tests + build) and is pulled + rebuilt on klaud-laptop for
next-day user testing.

## 9. Decisions Log (owner Q&A, 2026-08-04)

- Post-save: toast with „Otwórz PDF" button (no auto-open).
- Saved-document editing: full re-edit (5.3) **and** pre-save review (5.2) both wanted.
- New report during Q&A: auto-capture sometimes fires on an empty mat → §4.2 gate.
