# Compact Bilevel PDFs (CCITT G4) — Design Spec

**Date:** 2026-08-20 · **Author:** Paul (KaiPizz) + Claude · **Status:** Approved by owner (chat, 2026-08-20)

## 1. Context

Audit of the production library on klaud-laptop (`C:\Users\klaud\Contacts\Documents\Zeskanowane dokumenty`, 2026-08-20):

| Metric | Value |
|---|---|
| PDFs | 231 (3.75 GB total) |
| Files > 5 MB | **227 / 231** |
| Largest | `Umowy Archiwalne\A13.pdf` 111.7 MB (~53 pages), A12 98 MB, A09 94 MB |
| Typical thin document (3–6 pages) | 7–12 MB |

Root cause (`document.rs`, `pipeline.rs` at `8092398`): every page is warped onto an
A4 canvas of **2480×3508 px (300 dpi)**, kept **RGB**, and encoded **JPEG q91 →
≈2.1 MB/page**. `render_pdf` embeds those bytes verbatim (`DCTDecode/DeviceRGB`).
No colour reduction or bilevel step exists anywhere.

Measured on a real page from `Sektor-A\A10.pdf`:

| Encoding | KB/page | 50-page document |
|---|---|---|
| RGB JPEG q91 300 dpi (today) | 2100 | 105 MB |
| Gray JPEG q60 300 dpi | 690 | 34 MB |
| Gray JPEG q45 150 dpi | 214 | 10.7 MB |
| **1-bit 300 dpi CCITT G4** | **66** | **3.3 MB** |

Owner decisions: the limit is **per PDF file, ~5 MB**; most documents must fit,
very thick documents (100 pages ≈ 6.6 MB) may exceed it a little; **resolution
must not drop** (legibility first); **black-and-white by default**. Cloud/R2
upload is out of scope for this wave.

## 2. Goals / Non-Goals

**Goals**

1. New scans are stored as **1-bit 300 dpi pages, CCITT Group 4**, both in RAM
   and in the PDF → ≈66 KB/page, no resolution loss, readable by every PDF viewer
   natively (no JBIG2).
2. Existing PDFs (RGB DCT) still open, display, edit and re-save. Newly produced
   PDFs open, display, edit and re-save.
3. Crash-recovery sessions survive the format change (old sessions still restore;
   new sessions store G4 pages).
4. A global setting **„Tryb koloru”** (default *Czarno-biały*, option *Kolor*)
   for stamped/coloured documents, which keeps today's RGB JPEG path (q80).
5. The cloud sync path (`sync.rs`) keeps compiling and working against the
   existing backend (which accepts jpeg/png/webp only) by uploading a PNG for
   bilevel pages. It is not deployed in this wave.

**Non-goals**

- Recompressing the 231 existing PDFs (separate wave, needs backup; will reuse
  the extractor + binarizer from this spec).
- JBIG2, MRC, OCR, downsampling.
- Per-page or per-document colour mode (global setting only).

## 3. Design

### 3.1 Page representation (`document.rs`)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageEncoding {
    /// Baseline JPEG, RGB, as produced today (`Kolor` mode and legacy PDFs).
    Jpeg,
    /// CCITT Group 4 (T.6) raw stream, 1 bit/pixel, **0 = black** as in PDF
    /// `BlackIs1 false` (the fax crate's `Color::White`/`Black` map directly).
    G4,
}

pub struct ScannedPage {
    pub bytes: Vec<u8>,          // was `jpeg`
    pub encoding: PageEncoding,  // new
    pub review_image: RgbImage,  // unchanged (≤320 px thumbnail, made AFTER binarization so it shows what is saved)
    pub width: u32,
    pub height: u32,
}
```

One decoder for the whole app:

```rust
pub fn decode_page(bytes: &[u8], encoding: PageEncoding, width: u32, height: u32) -> Result<RgbImage, String>
```

- `Jpeg` → existing `decode_jpeg`.
- `G4` → `fax::decoder::decode_g4(bytes.iter().copied(), width, Some(height), |line| …)`,
  expanding `fax::decoder::pels` into a `GrayImage` then `to_rgb8()` (the egui
  texture path wants RGB(A) anyway). Fewer decoded lines than `height` or a decoder
  failure → `Err` (never a half page silently).

Callers that today read `page.jpeg` move to `decode_page` / `page.bytes`:
`review_viewport.rs` decode worker (job carries bytes+encoding+width+height),
`app.rs` editor texture + `session_write_page` + sync upload, `session.rs`,
`document.rs` extractor helpers (`page_from_jpeg_bytes` → `page_from_encoded`).

### 3.2 Binarization (`document.rs`, new `fn binarize_document(&RgbImage) -> GrayImage /* 0|255 */`)

Runs in the pipeline worker **after** `enhance_document` (which already flattens
uneven lighting per channel), so a simple local threshold is enough:

1. Luma (Rec. 601 weights) from the enhanced RGB.
2. **Sauvola threshold** on an integral image (sum + sum of squares):
   `T = m · (1 + k · (s / R − 1))`, window **41 px**, `k = 0.30`, `R = 128`.
   Pixel < T → black. Integral images make this O(pixels); ~8.7 MP ≈ 100–150 ms
   release on the Ryzen 5 laptop — acceptable inside the existing worker thread
   (capture latency budget unchanged: the UI never waits on it).
3. **Border cleanup**: black connected components that touch the canvas edge are
   removed (flood fill from every black border pixel, 4-connected). This erases the
   dark mat rim that the 1.8 % corner expansion drags in (visible in the prototype)
   without touching inner content; a component that is both border-touching and
   spans > 60 % of width or height (a real full-width table rule cut by the edge)
   is kept.
4. **Despeckle**: black components with ≤ 3 pixels are removed (sensor noise).
   Text diacritics (`ą ę ó ć ń`) at 300 dpi are ≥ 8 px, so they survive.

`Kolor` mode skips this function entirely.

### 3.3 Encoding (`document.rs::page_from_image` → split)

- `PageEncoding::G4`: `fax::encoder::Encoder::new(VecWriter::new())`, one
  `encode_line(row.iter().map(|v| if *v == 0 { Color::Black } else { Color::White }), width)`
  per row, then `finish()` → `VecWriter::finish()` → `bytes`. Target ≈ 40–120 KB/page.
- `PageEncoding::Jpeg` (Kolor): existing `JpegEncoder` but **q80** instead of q91
  (≈1.3 MB/page; colour is opt-in for the rare stamped/coloured document).

`process_page_with` gains a `ColorMode` parameter; `process_page` keeps its
signature for tests by defaulting to B&W.

### 3.4 PDF output (`render_pdf`)

Per page, depending on `page.encoding`:

| | Jpeg (unchanged) | G4 (new) |
|---|---|---|
| `ColorSpace` | `DeviceRGB` | `DeviceGray` |
| `BitsPerComponent` | 8 | **1** |
| `Filter` | `DCTDecode` | **`CCITTFaxDecode`** |
| `DecodeParms` | — | `<< /K -1 /Columns W /Rows H /BlackIs1 false >>` |

Everything else (A4 MediaBox, `q cm Do Q` content, `/Rotate`, Info marker
`skaner-dokumentow-editable-v1`) stays identical, so the round-trip guarantees hold:
the PDF carries the exact G4 bytes held in RAM → save is still decode-free, and
repeated edit → save cycles add no loss. Acceptance: `pdfimages -list` reports
`2480×3508 gray 1 1 ccitt`, and a 50-page G4 document is < 5 MB.

### 3.5 PDF input (`extract_pdf_pages`, `page_is_safe_to_edit`)

Returns `Vec<(Vec<u8>, PageEncoding, u32 /*w*/, u32 /*h*/, u8 /*turns*/)>` (a small
`ExtractedPage` struct). Acceptance rules extend symmetrically:

- lone `DCTDecode` + `DeviceRGB`/8 bpc → `Jpeg` (today's rule, untouched), **or**
- lone `CCITTFaxDecode` + `DeviceGray`/1 bpc + `DecodeParms.K == -1` +
  `BlackIs1` absent/false + `Columns/Rows == Width/Height` → `G4`.
  Any other DecodeParms (K ≥ 0, EncodedByteAlign, EndOfBlock false) → not ours →
  the existing "foreign PDF" error.

`page_is_safe_to_edit` keeps its A4 ±10 % dimension check for both encodings.

Editing a page loaded from an old RGB PDF and re-saving keeps it `Jpeg` (we never
re-encode what we did not re-process). Re-cropping any page (old or new) goes
through `Job::Reprocess` on the **original frame** when the session still has it;
when it does not (reopened PDF), reprocess warps the decoded page raster — a G4
page warped bilinearly yields gray edges, which the binarizer cleans again. Good
enough; no special casing.

### 3.6 Session / crash recovery (`session.rs`)

`PageMetadata` becomes format **2**:

```rust
struct PageMetadata {
    corners: [CropPoint; 4],
    #[serde(default)] quarter_turns: u8,
    #[serde(default)] format: u8,          // 0 legacy rotated jpeg, 1 jpeg+turns, 2 = has encoding/width/height
    #[serde(default)] encoding: u8,        // 0 = Jpeg, 1 = G4
    #[serde(default)] width: u32,
    #[serde(default)] height: u32,
}
```

Page file name stays `{id}.r{rev}.jpg` for Jpeg and becomes `{id}.r{rev}.g4` for
G4 (raw T.6 stream; it is only meaningful with the metadata, which is fine —
the orphan salvage path (`salvage_orphan_pages`) only ever rebuilt **Jpeg** pages
from bare files; for `.g4` without metadata it skips the page and counts it in
`skipped_pages`, exactly like a missing file today). `RecoveredPage.jpeg` →
`RecoveredPage.page: Option<(Vec<u8>, PageEncoding, u32, u32)>`. Format 0/1
manifests decode as before (`encoding` defaults to Jpeg; width/height are read
from the JPEG header as today).

`session_file_kind` / GC sweep learn the `.g4` suffix wherever `.jpg` is matched.

### 3.7 Settings + UI (`storage.rs`, `app.rs`)

`Settings.color_mode: Option<ColorMode>` (`ColorMode { BlackWhite, Color }`,
`None` ⇒ BlackWhite). Settings modal gains a row **„Tryb koloru”** with a two-option
combo („Czarno-biały (domyślnie)”, „Kolor”) next to the auto-capture toggle; value
is read when a `Job::New` is submitted (`submit(id, frame, color_mode)`), so it
applies to the next captured page, never retroactively.

Status text of a ready page (film strip tooltip) shows the page size
(`66 KB`) so the owner can see the gain without opening Explorer.

### 3.8 Cloud sync (`sync.rs`, `app.rs::spawn_scan_upload`)

`spawn_scan_upload(id, page)` now takes the page; for `G4` it encodes a **1-bit
PNG** (image crate `PngEncoder`, `ExtendedColorType::L1`, ≈115 KB) inside the
upload thread and sends `image/png`; for `Jpeg` it sends the bytes as today.
Not deployed (endpoint still 404 on Contabo); covered by a unit test that the
multipart part is built with the right MIME and a decodable PNG.

### 3.9 Dependencies

`Cargo.toml`: `fax = "0.3.0"` (no transitive deps, std only, 41 M downloads,
2026-07 release). No C/C++ toolchain change. `image` gains nothing new (PNG + JPEG
already enabled).

## 4. Error handling

- G4 encode/decode failure on a page → `PipelineEvent::PageFailed` with the
  Polish message `Nie można zakodować strony (G4)` / `Nie można odczytać strony`;
  the slot shows ⚠ like any processing failure today. Never falls back to JPEG
  silently (the owner would not notice a 2 MB page among 66 KB ones).
- Extractor sees a `CCITTFaxDecode` stream with unsupported parms → treated as
  a foreign PDF (existing path, read-only open in the system viewer).
- Session restore with format 2 but missing `width/height` → page skipped and
  counted, same as a corrupt JPEG today.

## 5. Testing

All existing 94 tests must stay green (`cargo test --release`, app closed). New:

| Area | Test |
|---|---|
| binarize | synthetic page (gray gradient background + black text strokes + 2-px noise + dark border rim): output is strictly {0,255}; rim gone; noise gone; strokes intact (pixel-count bounds). |
| binarize | real frame (`IRISCAN_TEST_FRAME`, ignored): G4 bytes 30–150 KB, black ratio 2–12 %. |
| G4 codec | encode → decode round-trip is pixel-identical for a random bilevel image (incl. all-white and all-black rows, odd width). |
| render_pdf | G4 page → `extract_pdf_pages` returns identical bytes + `G4` + dims (mirrors `extracts_pages_from_own_pdf`). |
| render_pdf | mixed document (Jpeg page + G4 page) round-trips both. |
| safe_to_edit | `CCITTFaxDecode` with `K 0` (G3) or `BlackIs1 true` → rejected as foreign. |
| session | format 2 G4 page restores with encoding+dims; format 1 manifest still restores as Jpeg; orphan `.g4` without metadata → skipped count. |
| pipeline | `Job::New` with `BlackWhite` yields `G4`, with `Color` yields `Jpeg` (dimensions follow quad orientation as before). |
| sync | G4 page → upload part is `image/png` and decodes to an L1 PNG of the right size. |

Manual acceptance on klaud-laptop (IRIScan, desktop session): scan a 5-page and
a thick (≥40 pages) document; check PDF size (< 0.5 MB / < 5 MB), open in Edge +
Acrobat Reader, zoom 400 % on small print and a signature; open the PDF back in the
app, rotate one page, save; kill the app mid-scan → restore dialog shows the pages;
switch „Tryb koloru” to Kolor and scan one stamped page.

## 6. Rollout

1. Implement on gm (`D:\scan-app`, branch `feat/bilevel-g4-20260820`), test
   `--release` on gm, merge to `main`, push to GitHub.
2. klaud-laptop (staff idle): `taskkill skaner-dokumentow` → `git pull` →
   `cargo build --release` (~7 min) → manual acceptance above.
3. Old library untouched; recompression is the next wave once the owner has
   lived with B&W output for a few days.
