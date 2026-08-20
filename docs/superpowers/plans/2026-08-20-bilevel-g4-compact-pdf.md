# Compact Bilevel PDFs (CCITT G4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** New scans become 1-bit 300-dpi CCITT G4 pages (~66 KB instead of ~2.1 MB) in RAM, session and PDF, so a 50-page document is ~3 MB; old RGB PDFs keep opening/editing; a global „Tryb koloru” setting keeps the colour path.

**Architecture:** A new `src/bilevel.rs` owns binarization (Sauvola + border cleanup + despeckle) and the G4 codec (crate `fax`). `document.rs` gains `PageEncoding {Jpeg, G4}` / `EncodedPage` / `ColorMode`, one `decode_page()` used by every consumer, a G4 branch in `render_pdf` and in the extractor. Session (format 2), review viewport, sync and settings are adapted to carry `EncodedPage` instead of raw JPEG bytes.

**Tech Stack:** Rust 2024, eframe/egui 0.35, image 0.25, imageproc 0.27, lopdf 0.44, **fax 0.3.0 (new)**, ron, serde. Build/test on gm (`D:\scan-app`, `%USERPROFILE%\.cargo\bin\cargo.exe`), Windows-only crate (nokhwa MSMF) — it does **not** compile on Linux.

**Spec:** `docs/superpowers/specs/2026-08-20-bilevel-g4-compact-pdf-design.md`

## Global Constraints

- Binarize at **300 dpi, never downsample**; Sauvola window 41 px, k = 0.20, R = 128; border-touching black components removed unless they span > 60 % of width or height; components ≤ 3 px removed.
- G4 bit convention: PDF `BlackIs1 false` → `fax::Color::Black`/`White` written directly; `GrayImage` pixels are strictly `0` (black) or `255` (white).
- PDF G4 XObject: `DeviceGray`, `BitsPerComponent 1`, `CCITTFaxDecode`, `DecodeParms << /K -1 /Columns W /Rows H /BlackIs1 false >>`. Everything else in `render_pdf` unchanged (A4 MediaBox, `q cm Do Q`, `/Rotate`, Keywords marker `skaner-dokumentow-editable-v1`).
- `Kolor` mode = RGB JPEG **q80**; `ColorMode::default()` = `BlackWhite`.
- No silent fallback from G4 to JPEG: codec failure → `PipelineEvent::PageFailed` with `Nie można zakodować strony (G4).`
- All Polish UI strings; all existing 94 tests stay green; `cargo clippy --release` clean.
- Git identity for every commit: `Kaipizz <97230060+KaiPizz@users.noreply.github.com>` (already repo-local on gm). Never commit on `main` — branch `feat/bilevel-g4-20260820`.
- Workflow per task: edit in the local clone (`$SCRATCH/scan-app`), `scp` the touched files to gm, run tests on gm, commit **on gm**. Helper scripts are created in Task 0.

---

### Task 0: Working environment (gm branch + helper scripts)

**Files:**
- Create: `$SCRATCH/gm-push.sh`, `$SCRATCH/gm-test.sh`, `$SCRATCH/gm-commit.sh` (scratch only, not in repo)

`$SCRATCH` = `/var/tmp/rf-build/claude-1000/-var-www-www-enail/9bd5cc3e-db8f-4d63-aa45-1f3eb9b53341/scratchpad`. Local clone = `$SCRATCH/scan-app` (branch `feat/bilevel-g4-20260820`, spec commit `e8d5269`).

- [ ] **Step 1: Create the branch on gm from `main` (`8092398`), and put spec + plan there**

```bash
cd $SCRATCH/scan-app
git add docs/superpowers/plans/2026-08-20-bilevel-g4-compact-pdf.md
git commit -m "docs: implementation plan for compact bilevel PDFs"
git bundle create $SCRATCH/docs.bundle 8092398..feat/bilevel-g4-20260820
scp $SCRATCH/docs.bundle gm:D:/scan-app/docs.bundle
ssh gm "cd /d D:\scan-app && git status --short && git fetch docs.bundle feat/bilevel-g4-20260820:feat/bilevel-g4-20260820 && git checkout feat/bilevel-g4-20260820 && git log --oneline -3 && del docs.bundle"
```
Expected: `git status --short` prints nothing before the fetch (tree clean at `8092398`); log shows the plan + spec commits on top of `8092398`.

- [ ] **Step 2: Helper scripts**

```bash
cat > $SCRATCH/gm-push.sh <<'EOF'
#!/usr/bin/env bash
# usage: gm-push.sh <repo-relative paths...>   (copies from local clone to gm D:\scan-app)
set -euo pipefail
cd /var/tmp/rf-build/claude-1000/-var-www-www-enail/9bd5cc3e-db8f-4d63-aa45-1f3eb9b53341/scratchpad/scan-app
for f in "$@"; do scp -q "$f" "gm:D:/scan-app/$f"; echo "pushed $f"; done
EOF
cat > $SCRATCH/gm-test.sh <<'EOF'
#!/usr/bin/env bash
# usage: gm-test.sh [cargo test args...]   e.g. gm-test.sh bilevel   |  gm-test.sh -- --ignored
set -uo pipefail
ssh gm "cd /d D:\scan-app && %USERPROFILE%\.cargo\bin\cargo.exe test --release $* 2>&1" | tail -n 60
EOF
cat > $SCRATCH/gm-commit.sh <<'EOF'
#!/usr/bin/env bash
# usage: gm-commit.sh "<message>"   (git add -A + commit on gm, identity is repo-local there)
set -euo pipefail
printf '%s\n' "$1" > /tmp/gm-msg.txt
scp -q /tmp/gm-msg.txt gm:D:/scan-app/.gm-msg.txt
ssh gm "cd /d D:\scan-app && git add -A && git reset -q .gm-msg.txt && git commit -q -F .gm-msg.txt && del .gm-msg.txt && git log --oneline -1 && git log -1 --format=%an^<%ae^>"
EOF
chmod +x $SCRATCH/gm-*.sh
```

- [ ] **Step 3: Baseline — prove the toolchain works before changing anything**

Run: `$SCRATCH/gm-test.sh 2>&1 | tail -5`
Expected: `test result: ok. 94 passed` (or 93 passed + `second_instance_lock_is_rejected` failing **only** if the app is open on gm — close it with `ssh gm "taskkill /im skaner-dokumentow.exe /f"` and rerun).

---

### Task 1: G4 codec (`src/bilevel.rs`)

**Files:**
- Modify: `Cargo.toml` (add `fax = "0.3.0"` under `[dependencies]`)
- Modify: `src/main.rs:3-15` (add `mod bilevel;`)
- Create: `src/bilevel.rs`

**Interfaces:**
- Produces: `pub fn encode_g4(image: &GrayImage) -> Vec<u8>` (any pixel < 128 is black), `pub fn decode_g4(bytes: &[u8], width: u32, height: u32) -> Result<GrayImage, String>` (pixels exactly 0/255; `Err` on truncated/invalid data).

- [ ] **Step 1: Add the dependency and the module**

`Cargo.toml` — after the `eframe` line:
```toml
fax = "0.3.0"
```
`src/main.rs` — after `mod autocapture;`:
```rust
mod bilevel;
```

- [ ] **Step 2: Write the failing tests**

Create `src/bilevel.rs`:
```rust
//! Bilevel (1-bit) page support: binarization and the CCITT Group 4 codec.
//!
//! Bit convention everywhere in this module: a `GrayImage` pixel is `0`
//! (ink) or `255` (paper). On the wire (`encode_g4`/`decode_g4`) black is
//! `fax::Color::Black`, which is what PDF `CCITTFaxDecode` with
//! `/BlackIs1 false` expects.

use fax::decoder::{decode_g4 as fax_decode_g4, pels};
use fax::encoder::Encoder;
use fax::{Color, VecWriter};
use image::{GrayImage, Luma};

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(width: u32, height: u32) -> GrayImage {
        GrayImage::from_fn(width, height, |x, y| {
            if (x / 3 + y / 5) % 2 == 0 { Luma([0]) } else { Luma([255]) }
        })
    }

    #[test]
    fn g4_round_trip_is_pixel_identical() {
        let mut image = checker(37, 11); // odd width, mixed rows
        for x in 0..37 { image.put_pixel(x, 0, Luma([255])); } // all-white row
        for x in 0..37 { image.put_pixel(x, 1, Luma([0])); }   // all-black row
        let bytes = encode_g4(&image);
        assert!(!bytes.is_empty());
        let decoded = decode_g4(&bytes, 37, 11).expect("decode");
        assert_eq!(decoded.dimensions(), (37, 11));
        assert_eq!(decoded.as_raw(), image.as_raw());
    }

    #[test]
    fn g4_treats_any_dark_value_as_black() {
        let mut image = GrayImage::from_pixel(8, 1, Luma([200]));
        image.put_pixel(3, 0, Luma([127]));
        let decoded = decode_g4(&encode_g4(&image), 8, 1).expect("decode");
        assert_eq!(decoded.get_pixel(3, 0), &Luma([0]));
        assert_eq!(decoded.get_pixel(2, 0), &Luma([255]));
    }

    #[test]
    fn g4_white_page_compresses_to_a_few_bytes() {
        let image = GrayImage::from_pixel(2480, 3508, Luma([255]));
        let bytes = encode_g4(&image);
        assert!(bytes.len() < 2048, "white A4 page took {} bytes", bytes.len());
        assert_eq!(decode_g4(&bytes, 2480, 3508).expect("decode").as_raw(), image.as_raw());
    }

    #[test]
    fn truncated_g4_is_an_error_not_a_half_page() {
        let bytes = encode_g4(&checker(64, 64));
        let cut = &bytes[..bytes.len() / 3];
        assert!(decode_g4(cut, 64, 64).is_err());
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `$SCRATCH/gm-push.sh Cargo.toml src/main.rs src/bilevel.rs && $SCRATCH/gm-test.sh bilevel`
Expected: compile error `cannot find function encode_g4` (and the first `cargo` run downloads `fax`).

- [ ] **Step 4: Implement the codec**

Insert above the `#[cfg(test)]` block in `src/bilevel.rs`:
```rust
/// Encodes a bilevel image as a raw CCITT Group 4 (T.6) stream — exactly the
/// bytes a PDF `CCITTFaxDecode` filter with `/K -1 /BlackIs1 false` expects.
/// Any pixel darker than 128 is ink.
pub fn encode_g4(image: &GrayImage) -> Vec<u8> {
    let width = image.width();
    let mut encoder = Encoder::new(VecWriter::with_capacity(width as usize * image.height() as usize / 16));
    for row in image.rows() {
        let pels = row.map(|pixel| if pixel.0[0] < 128 { Color::Black } else { Color::White });
        // VecWriter's error type is `Infallible`.
        let Ok(()) = encoder.encode_line(pels, width);
    }
    let Ok(writer) = encoder.finish();
    writer.finish()
}

/// Decodes a raw G4 stream of known dimensions. Fewer lines than `height`,
/// or a decoder error, is a hard error: a half page must never pass as a page.
pub fn decode_g4(bytes: &[u8], width: u32, height: u32) -> Result<GrayImage, String> {
    if width == 0 || height == 0 {
        return Err("Strona ma zerowy rozmiar.".to_owned());
    }
    let mut image = GrayImage::from_pixel(width, height, Luma([255]));
    let mut lines = 0_u32;
    let status = fax_decode_g4(bytes.iter().copied(), width, Some(height), |transitions| {
        if lines < height {
            for (x, color) in pels(transitions, width).enumerate() {
                if color == Color::Black {
                    image.put_pixel(x as u32, lines, Luma([0]));
                }
            }
        }
        lines += 1;
    });
    if status.is_none() {
        return Err("Nie można odczytać strony (uszkodzone dane G4).".to_owned());
    }
    if lines < height {
        return Err(format!(
            "Nie można odczytać strony (G4: {lines} z {height} wierszy)."
        ));
    }
    Ok(image)
}
```
Note: `fax::decoder::decode_g4` pads missing trailing all-white lines up to `height` itself (it calls the callback with an empty transition list), so a short stream that still ends with a proper EOFB decodes to a white-padded page — only a stream that *errors* (truncated mid-code) is rejected. The truncation test cuts inside the data, which makes the decoder fail. If on gm the truncated test unexpectedly passes decoding (decoder tolerant), change the test to cut the stream to 8 bytes.

- [ ] **Step 5: Run tests to verify they pass**

Run: `$SCRATCH/gm-push.sh src/bilevel.rs && $SCRATCH/gm-test.sh bilevel`
Expected: `4 passed`.

- [ ] **Step 6: Commit**

Run: `$SCRATCH/gm-commit.sh "feat(bilevel): CCITT G4 encode/decode via the fax crate"`
Then copy the lock file back so the local clone matches: `scp gm:D:/scan-app/Cargo.lock $SCRATCH/scan-app/Cargo.lock`.

---

### Task 2: Binarization (`src/bilevel.rs`)

**Files:**
- Modify: `src/bilevel.rs`

**Interfaces:**
- Produces: `pub fn binarize(image: &RgbImage) -> GrayImage` (strict 0/255), plus private `sauvola_threshold`, `remove_border_and_specks`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` of `src/bilevel.rs`:
```rust
    use image::{Rgb, RgbImage};

    /// Synthetic enhanced page: light gradient paper, black strokes, 2-px
    /// specks, and a dark rim on the right edge (what the mat leaves after the
    /// 1.8 % corner expansion).
    fn synthetic_page() -> RgbImage {
        let (w, h) = (600_u32, 800_u32);
        let mut page = RgbImage::from_fn(w, h, |x, _| {
            let v = 215 + (x * 40 / w) as u8; // 215..255 gradient
            Rgb([v, v, v])
        });
        // Text-like strokes: 8 rows of 12-px tall bars.
        for row in 0..8 {
            let y0 = 60 + row * 80;
            for x in (40..500).step_by(30) {
                for dy in 0..12 { for dx in 0..18 { page.put_pixel(x + dx, y0 + dy, Rgb([20, 20, 20])); } }
            }
        }
        // Specks (2x1 px) scattered on the paper.
        for y in (30..h).step_by(97) { page.put_pixel(25, y, Rgb([0, 0, 0])); page.put_pixel(26, y, Rgb([0, 0, 0])); }
        // Dark rim on the right edge, 6 px wide, full height.
        for y in 0..h { for x in w - 6..w { page.put_pixel(x, y, Rgb([30, 30, 30])); } }
        page
    }

    fn black_count(image: &GrayImage) -> usize {
        image.pixels().filter(|p| p.0[0] == 0).count()
    }

    #[test]
    fn binarize_is_strictly_bilevel() {
        let out = binarize(&synthetic_page());
        assert!(out.pixels().all(|p| p.0[0] == 0 || p.0[0] == 255));
        assert_eq!(out.dimensions(), (600, 800));
    }

    #[test]
    fn binarize_keeps_strokes_and_drops_rim_and_specks() {
        let out = binarize(&synthetic_page());
        // Every stroke pixel survives (8 rows × 16 bars × 18×12 px = 27_648).
        let mut kept = 0;
        for row in 0..8 { let y0 = 60 + row * 80; for x in (40..500).step_by(30) {
            for dy in 0..12 { for dx in 0..18 { if out.get_pixel(x + dx, y0 + dy).0[0] == 0 { kept += 1; } } } } }
        assert!(kept >= 27_648 * 97 / 100, "strokes lost: kept {kept}");
        // The rim is gone (it touched the border and is narrow).
        for y in (0..800).step_by(50) { for x in 594..600 {
            assert_eq!(out.get_pixel(x, y).0[0], 255, "rim survived at ({x},{y})"); } }
        // Specks are gone.
        for y in (30..800).step_by(97) { assert_eq!(out.get_pixel(25, y).0[0], 255, "speck at y={y}"); }
        // Total ink ≈ strokes only (no more than +3 %).
        assert!(black_count(&out) <= 27_648 * 103 / 100, "extra ink: {}", black_count(&out));
    }

    #[test]
    fn binarize_keeps_a_full_width_rule_touching_the_border() {
        // A 1-px rule across the full width (0.33 % of the height) touches the
        // border on both ends — it is content, not the mat rim.
        let mut page = RgbImage::from_pixel(400, 300, Rgb([240, 240, 240]));
        for x in 0..400 { page.put_pixel(x, 150, Rgb([10, 10, 10])); }
        let out = binarize(&page);
        assert_eq!(out.get_pixel(200, 150).0[0], 0, "table rule was erased");
        assert_eq!(out.get_pixel(0, 150).0[0], 0);
    }

    #[test]
    fn binarize_blank_page_is_white() {
        let page = RgbImage::from_pixel(300, 200, Rgb([235, 238, 240]));
        assert_eq!(black_count(&binarize(&page)), 0);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `$SCRATCH/gm-push.sh src/bilevel.rs && $SCRATCH/gm-test.sh bilevel`
Expected: compile error `cannot find function binarize`.

- [ ] **Step 3: Implement Sauvola + cleanup**

Insert above `#[cfg(test)]` in `src/bilevel.rs` (after the codec):
```rust
use image::RgbImage;
use imageproc::region_labelling::{Connectivity, connected_components};

const SAUVOLA_WINDOW: u32 = 41; // px at 300 dpi
const SAUVOLA_K: f64 = 0.20;
const SAUVOLA_R: f64 = 128.0;
/// A border-touching black component is the dark mat rim (the 1.8 % corner
/// expansion drags it in) unless it is a thin rule: ≥ 60 % of one side long
/// and ≤ 0.4 % of the other side thick (≤ 14 px at A4/300 dpi). The rim is
/// ≥ 1 % thick, so it is dropped; a table rule cut by the crop survives.
const BORDER_KEEP_SPAN: f64 = 0.60;
const RULE_MAX_THICKNESS: f64 = 0.004;
const SPECK_MAX_PX: u32 = 3;

/// Turns an already lighting-flattened RGB page into strict 0/255 ink/paper.
pub fn binarize(image: &RgbImage) -> GrayImage {
    let gray = GrayImage::from_fn(image.width(), image.height(), |x, y| {
        let p = image.get_pixel(x, y).0;
        let luma = 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32;
        Luma([luma.round().clamp(0.0, 255.0) as u8])
    });
    let mut bilevel = sauvola_threshold(&gray);
    remove_border_and_specks(&mut bilevel);
    bilevel
}

/// Sauvola: T = m · (1 + k · (s / R − 1)) over a square window, via integral
/// images of the sum and the sum of squares (O(pixels), no per-pixel loops
/// over the window).
fn sauvola_threshold(gray: &GrayImage) -> GrayImage {
    let (w, h) = (gray.width() as usize, gray.height() as usize);
    let stride = w + 1;
    let mut sum = vec![0_u64; (w + 1) * (h + 1)];
    let mut sq = vec![0_u64; (w + 1) * (h + 1)];
    for y in 0..h {
        let mut row_sum = 0_u64;
        let mut row_sq = 0_u64;
        for x in 0..w {
            let v = gray.as_raw()[y * w + x] as u64;
            row_sum += v;
            row_sq += v * v;
            sum[(y + 1) * stride + x + 1] = sum[y * stride + x + 1] + row_sum;
            sq[(y + 1) * stride + x + 1] = sq[y * stride + x + 1] + row_sq;
        }
    }
    let r = (SAUVOLA_WINDOW / 2) as isize;
    let mut out = GrayImage::from_pixel(gray.width(), gray.height(), Luma([255]));
    for y in 0..h {
        let y0 = (y as isize - r).max(0) as usize;
        let y1 = (y as isize + r + 1).min(h as isize) as usize;
        for x in 0..w {
            let x0 = (x as isize - r).max(0) as usize;
            let x1 = (x as isize + r + 1).min(w as isize) as usize;
            let n = ((y1 - y0) * (x1 - x0)) as f64;
            let s = (sum[y1 * stride + x1] + sum[y0 * stride + x0]
                - sum[y0 * stride + x1]
                - sum[y1 * stride + x0]) as f64;
            let s2 = (sq[y1 * stride + x1] + sq[y0 * stride + x0]
                - sq[y0 * stride + x1]
                - sq[y1 * stride + x0]) as f64;
            let mean = s / n;
            let var = (s2 / n - mean * mean).max(0.0);
            let threshold = mean * (1.0 + SAUVOLA_K * (var.sqrt() / SAUVOLA_R - 1.0));
            if (gray.as_raw()[y * w + x] as f64) < threshold {
                out.as_mut()[y * w + x] = 0;
            }
        }
    }
    out
}

/// Drops (1) black components touching the image border that do not span
/// ≥ 60 % of the width or height, and (2) components of ≤ 3 pixels.
fn remove_border_and_specks(bilevel: &mut GrayImage) {
    let (w, h) = bilevel.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let labels = connected_components(bilevel, Connectivity::Four, Luma([255]));
    let count = labels.pixels().map(|p| p.0[0]).max().unwrap_or(0) as usize + 1;
    // Per label: pixel count, touches border, bbox.
    let mut size = vec![0_u32; count];
    let mut border = vec![false; count];
    let mut min_x = vec![u32::MAX; count];
    let mut max_x = vec![0_u32; count];
    let mut min_y = vec![u32::MAX; count];
    let mut max_y = vec![0_u32; count];
    for (x, y, label) in labels.enumerate_pixels() {
        let l = label.0[0] as usize;
        if l == 0 {
            continue;
        }
        size[l] += 1;
        if x == 0 || y == 0 || x == w - 1 || y == h - 1 {
            border[l] = true;
        }
        min_x[l] = min_x[l].min(x);
        max_x[l] = max_x[l].max(x);
        min_y[l] = min_y[l].min(y);
        max_y[l] = max_y[l].max(y);
    }
    let mut drop = vec![false; count];
    for l in 1..count {
        if size[l] == 0 {
            continue;
        }
        if size[l] <= SPECK_MAX_PX {
            drop[l] = true;
            continue;
        }
        if border[l] {
            let span_x = (max_x[l] - min_x[l] + 1) as f64 / w as f64;
            let span_y = (max_y[l] - min_y[l] + 1) as f64 / h as f64;
            let thin_rule = (span_x >= BORDER_KEEP_SPAN && span_y <= RULE_MAX_THICKNESS)
                || (span_y >= BORDER_KEEP_SPAN && span_x <= RULE_MAX_THICKNESS);
            if !thin_rule {
                drop[l] = true;
            }
        }
    }
    for (x, y, label) in labels.enumerate_pixels() {
        let l = label.0[0] as usize;
        if l != 0 && drop[l] {
            bilevel.put_pixel(x, y, Luma([255]));
        }
    }
}
```
Also update the spec §3.2 sentence in the same commit: "a component that is both border-touching and spans > 60 % of width or height (a real full-width table rule cut by the edge) is kept" → "a border-touching component is kept only if it is a thin rule: ≥ 60 % of one side long and ≤ 0.4 % of the other side thick".

- [ ] **Step 4: Run tests to verify they pass**

Run: `$SCRATCH/gm-push.sh src/bilevel.rs && $SCRATCH/gm-test.sh bilevel`
Expected: `8 passed`. If `binarize_keeps_strokes_and_drops_rim_and_specks` fails on the `kept` bound, print `kept`/`black_count` and check that Sauvola's window (41) vs stroke height (12) isn't hollowing strokes — if strokes are hollow, lower `SAUVOLA_K` to 0.20 and re-run; do not change the test bounds.

- [ ] **Step 5: Real-frame timing + size probe (ignored test, diagnostic only)**

Append to `mod tests`:
```rust
    /// IRISCAN_TEST_FRAME=<frame.png|jpg> cargo test --release bilevel_real_frame -- --ignored --nocapture
    #[test]
    #[ignore = "diagnostyka na prawdziwej klatce"]
    fn bilevel_real_frame() {
        let input = std::env::var("IRISCAN_TEST_FRAME").expect("IRISCAN_TEST_FRAME");
        let frame = image::open(input).expect("klatka").to_rgb8();
        let corners = crate::document::detect_document_corners(&frame);
        let page = crate::document::process_page_with(&frame, corners, true, crate::document::ColorMode::BlackWhite)
            .expect("strona");
        let started = std::time::Instant::now();
        let _ = crate::document::decode_page(&page.encoded()).expect("decode");
        println!("bytes={} KB decode={:?}", page.bytes.len() / 1024, started.elapsed());
        assert!(page.bytes.len() < 150 * 1024, "strona za duża: {} B", page.bytes.len());
    }
```
**Do not add this test yet** — it uses `ColorMode`, `process_page_with(…, mode)`, `encoded()` and `decode_page`, which only exist after Task 3. Task 3 Step 3 tells you to paste it then; it is listed here so the binarization work is documented in one place.

- [ ] **Step 6: Commit**

Run: `$SCRATCH/gm-commit.sh "feat(bilevel): Sauvola binarization with rim cleanup and despeckle"`

---

### Task 3: Page types, G4 pipeline path, PDF write/read (`document.rs`, `pipeline.rs`, call sites)

**Files:**
- Modify: `src/document.rs` (types at :28-37, `process_page*` :196-216, `render_pdf` :321-420, `page_from_image` :424-439, `extract_pdf_pages` :445-564, `page_from_jpeg_bytes`/`pages_from_extracted` :813-840, tests)
- Modify: `src/pipeline.rs` (Job/New carries `ColorMode`, `try_submit`/`submit_reprocess` signatures, tests)
- Modify: `src/app.rs` (mechanical: `page.jpeg` → `page.bytes`; `page_from_jpeg_bytes` → `page_from_encoded`; pipeline calls pass `ColorMode::Color` for now)
- Modify: `src/review_viewport.rs:130-170,381-390` (accept `&EncodedPage`, decode via `decode_page`)

**Interfaces:**
- Produces (all `pub` in `document.rs`):
  ```rust
  pub enum PageEncoding { Jpeg, G4 }            // Copy, Eq, Debug, Serialize, Deserialize
  pub enum ColorMode { BlackWhite, Color }       // Copy, Eq, Debug, Serialize, Deserialize, Default = BlackWhite
  pub struct EncodedPage { pub bytes: Vec<u8>, pub encoding: PageEncoding, pub width: u32, pub height: u32 } // Clone, Debug, PartialEq, Eq
  pub struct ScannedPage { pub bytes: Vec<u8>, pub encoding: PageEncoding, pub review_image: RgbImage, pub width: u32, pub height: u32 }
  impl ScannedPage { pub fn encoded(&self) -> EncodedPage }
  pub fn decode_page(page: &EncodedPage) -> Result<RgbImage, String>
  pub fn page_from_encoded(page: EncodedPage) -> Result<ScannedPage, String>
  pub fn process_page(image, corners) -> Result<ScannedPage, String>                      // = process_page_with(image, corners, true, ColorMode::default())
  pub fn process_page_with(image, corners, expand_detected: bool, mode: ColorMode) -> Result<ScannedPage, String>
  pub fn extract_pdf_pages(path) -> Result<Vec<(EncodedPage, u8)>, String>
  pub fn pages_from_extracted(pages: Vec<(EncodedPage, u8)>) -> Result<Vec<(ScannedPage, u8)>, String>
  ```
- `pipeline.rs`: `try_submit(&self, id, frame, mode: ColorMode) -> bool`, `submit_reprocess(&self, id, frame, corners, mode: ColorMode) -> bool`.
- `review_viewport.rs`: `ensure_page(&mut self, context, key, page: &EncodedPage, placeholder)` (the `page_px` argument is gone — derived from `page.width/height`).

- [ ] **Step 1: Write the failing tests in `document.rs`**

Add to `mod tests` in `src/document.rs` (below `extracts_pages_from_own_pdf`):
```rust
    fn bilevel_page() -> ScannedPage {
        let mut image = RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([245, 245, 245]));
        for y in 400..440 { for x in 300..1800 { image.put_pixel(x, y, Rgb([15, 15, 15])); } }
        page_from_image(image, ColorMode::BlackWhite).expect("strona G4")
    }

    #[test]
    fn black_white_mode_produces_small_g4_pages() {
        let page = bilevel_page();
        assert_eq!(page.encoding, PageEncoding::G4);
        assert!(page.bytes.len() < 40 * 1024, "G4 page {} B", page.bytes.len());
        let decoded = decode_page(&page.encoded()).expect("decode");
        assert_eq!(decoded.dimensions(), (A4_WIDTH_PX, A4_HEIGHT_PX));
        assert_eq!(decoded.get_pixel(1000, 420), &Rgb([0, 0, 0]));
        assert_eq!(decoded.get_pixel(1000, 1000), &Rgb([255, 255, 255]));
        // The strip thumbnail reflects the bilevel result, not the colour input.
        assert!(page.review_image.pixels().all(|p| p.0[0] == p.0[1] && p.0[1] == p.0[2]));
    }

    #[test]
    fn color_mode_keeps_jpeg() {
        let image = RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([245, 200, 200]));
        let page = page_from_image(image, ColorMode::Color).expect("strona JPEG");
        assert_eq!(page.encoding, PageEncoding::Jpeg);
        assert!(page.bytes.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn g4_pdf_round_trips_exact_bytes_and_dimensions() {
        let page = bilevel_page();
        let path = std::env::temp_dir().join(format!("skaner-dokumentow-g4-{}.pdf", std::process::id()));
        save_pdf(&path, &[(&page, 1)]).expect("zapis PDF");
        let bytes = std::fs::read(&path).expect("odczyt PDF");
        assert!(bytes.windows(b"/CCITTFaxDecode".len()).any(|w| w == b"/CCITTFaxDecode"));
        assert!(bytes.windows(b"/BlackIs1 false".len()).any(|w| w == b"/BlackIs1 false"));
        let extracted = extract_pdf_pages(&path).expect("ekstrakcja");
        std::fs::remove_file(&path).expect("usunięcie testowego PDF");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].1, 1, "obrót z /Rotate");
        assert_eq!(extracted[0].0, page.encoded(), "bajty G4, kodowanie i wymiary muszą wrócić bez zmian");
        let reloaded = pages_from_extracted(extracted).expect("strony");
        assert_eq!(reloaded[0].0.encoding, PageEncoding::G4);
        assert_eq!((reloaded[0].0.width, reloaded[0].0.height), (A4_WIDTH_PX, A4_HEIGHT_PX));
    }

    #[test]
    fn mixed_jpeg_and_g4_document_round_trips() {
        let colour = page_from_image(RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([245, 245, 245])), ColorMode::Color).expect("jpeg");
        let bilevel = bilevel_page();
        let path = std::env::temp_dir().join(format!("skaner-dokumentow-mixed-{}.pdf", std::process::id()));
        save_pdf(&path, &[(&colour, 0), (&bilevel, 0)]).expect("zapis PDF");
        let extracted = extract_pdf_pages(&path).expect("ekstrakcja");
        std::fs::remove_file(&path).expect("usunięcie testowego PDF");
        assert_eq!(extracted[0].0.encoding, PageEncoding::Jpeg);
        assert_eq!(extracted[0].0.bytes, colour.bytes);
        assert_eq!(extracted[1].0.encoding, PageEncoding::G4);
        assert_eq!(extracted[1].0.bytes, bilevel.bytes);
    }

    /// Rewrites the single image stream's DecodeParms of a freshly saved G4 PDF.
    fn save_g4_pdf_with_parms(name: &str, parms: lopdf::Dictionary) -> std::path::PathBuf {
        let page = bilevel_page();
        let path = std::env::temp_dir().join(format!("skaner-dokumentow-{name}-{}.pdf", std::process::id()));
        save_pdf(&path, &[(&page, 0)]).expect("zapis PDF");
        let mut document = lopdf::Document::load(&path).expect("load");
        let ids: Vec<lopdf::ObjectId> = document
            .objects
            .iter()
            .filter(|(_, object)| matches!(object, lopdf::Object::Stream(stream) if stream.dict.has(b"DecodeParms")))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(ids.len(), 1);
        if let Ok(lopdf::Object::Stream(stream)) = document.get_object_mut(ids[0]) {
            stream.dict.set("DecodeParms", lopdf::Object::Dictionary(parms));
        }
        document.save(&path).expect("save");
        path
    }

    #[test]
    fn g3_or_blackis1_streams_are_foreign() {
        use lopdf::dictionary;
        let g3 = save_g4_pdf_with_parms("g3", dictionary! { "K" => 0_i64, "Columns" => i64::from(A4_WIDTH_PX), "Rows" => i64::from(A4_HEIGHT_PX), "BlackIs1" => false });
        assert!(extract_pdf_pages(&g3).is_err(), "K=0 (G3) must not be treated as our format");
        std::fs::remove_file(g3).expect("cleanup");
        let inverted = save_g4_pdf_with_parms("blackis1", dictionary! { "K" => -1_i64, "Columns" => i64::from(A4_WIDTH_PX), "Rows" => i64::from(A4_HEIGHT_PX), "BlackIs1" => true });
        assert!(extract_pdf_pages(&inverted).is_err(), "BlackIs1 true must not be treated as our format");
        std::fs::remove_file(inverted).expect("cleanup");
    }

    #[test]
    fn decode_page_rejects_corrupt_g4() {
        let page = EncodedPage { bytes: vec![0x00, 0x01, 0x02], encoding: PageEncoding::G4, width: 64, height: 64 };
        assert!(decode_page(&page).is_err());
    }
```
Also update existing tests for the rename (`page.jpeg` → `page.bytes`, `pages[0].0` is now `EncodedPage` → compare `.bytes`; `page_from_image(x)` → `page_from_image(x, ColorMode::Color)` in the existing JPEG-oriented tests `extracts_pages_from_own_pdf`, `rotation_round_trips_as_metadata_without_touching_bytes`, `saved_pdf_embeds_the_exact_jpeg_bytes`, `safely_imports_full_page_pdf_from_legacy_app_version`, `rejects_unmarked_pdf_that_is_not_a_full_a4_page`, `saving_pdf_replaces_an_existing_target`, `writes_a_valid_pdf`, the test at :1480 (`pages_from_extracted(vec![(page.encoded(), 0), (EncodedPage { bytes: b"not-a-jpeg".to_vec(), encoding: PageEncoding::Jpeg, width: 80, height: 120 }, 1)])`), and `image::load_from_memory(&pages[0].0)` → `&pages[0].0.bytes`).

- [ ] **Step 2: Run tests to verify they fail**

Run: `$SCRATCH/gm-push.sh src/document.rs && $SCRATCH/gm-test.sh document`
Expected: compile errors (`ColorMode`, `PageEncoding`, `EncodedPage` not found).

- [ ] **Step 3: Implement the types and codec plumbing in `document.rs`**

Replace `pub struct ScannedPage {…}` (:29-37) with:
```rust
/// How a page's bytes are encoded — in RAM, in the session and in the PDF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PageEncoding {
    /// Baseline JPEG, RGB (colour mode and every PDF saved before 2026-08).
    Jpeg,
    /// Raw CCITT Group 4 stream, 1 bit/pixel, black = `fax::Color::Black`
    /// (PDF `/BlackIs1 false`).
    G4,
}

/// Owner-facing colour setting; `BlackWhite` is the default because it
/// shrinks a page from ~2 MB to ~66 KB without losing resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ColorMode {
    #[default]
    BlackWhite,
    Color,
}

/// The persisted form of a page: exactly what goes into the PDF XObject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedPage {
    pub bytes: Vec<u8>,
    pub encoding: PageEncoding,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
pub struct ScannedPage {
    pub bytes: Vec<u8>,
    pub encoding: PageEncoding,
    /// Small thumbnail (≤`STRIP_THUMB_PX`) for the film strip and as the
    /// review placeholder while the full page decodes in the background.
    pub review_image: RgbImage,
    pub width: u32,
    pub height: u32,
}

impl ScannedPage {
    pub fn encoded(&self) -> EncodedPage {
        EncodedPage {
            bytes: self.bytes.clone(),
            encoding: self.encoding,
            width: self.width,
            height: self.height,
        }
    }
}

/// The one decoder every consumer (review, session restore, sync) uses.
pub fn decode_page(page: &EncodedPage) -> Result<RgbImage, String> {
    match page.encoding {
        PageEncoding::Jpeg => decode_jpeg(&page.bytes),
        PageEncoding::G4 => crate::bilevel::decode_g4(&page.bytes, page.width, page.height)
            .map(|gray| DynamicImage::ImageLuma8(gray).to_rgb8()),
    }
}
```
`process_page` / `process_page_with` (:196-216):
```rust
pub fn process_page(image: &RgbImage, corners: [CropPoint; 4]) -> Result<ScannedPage, String> {
    process_page_with(image, corners, true, ColorMode::default())
}

pub fn process_page_with(
    image: &RgbImage,
    corners: [CropPoint; 4],
    expand_detected: bool,
    mode: ColorMode,
) -> Result<ScannedPage, String> {
    let mut output = warp_only(image, corners, expand_detected)?;
    enhance_document(&mut output);
    page_from_image(output, mode)
}
```
`page_from_image` (:424-439):
```rust
fn page_from_image(image: RgbImage, mode: ColorMode) -> Result<ScannedPage, String> {
    let width = image.width();
    let height = image.height();
    match mode {
        ColorMode::Color => {
            let review_image = resize_to_fit(&image, STRIP_THUMB_PX, STRIP_THUMB_PX, imageops::FilterType::Lanczos3);
            let mut bytes = Vec::new();
            JpegEncoder::new_with_quality(&mut bytes, COLOR_JPEG_QUALITY)
                .encode_image(&image)
                .map_err(|error| format!("Nie można skompresować zeskanowanej strony: {error}"))?;
            Ok(ScannedPage { bytes, encoding: PageEncoding::Jpeg, review_image, width, height })
        }
        ColorMode::BlackWhite => {
            let bilevel = crate::bilevel::binarize(&image);
            let bytes = crate::bilevel::encode_g4(&bilevel);
            if bytes.is_empty() {
                return Err("Nie można zakodować strony (G4).".to_owned());
            }
            let preview = DynamicImage::ImageLuma8(bilevel).to_rgb8();
            let review_image = resize_to_fit(&preview, STRIP_THUMB_PX, STRIP_THUMB_PX, imageops::FilterType::Lanczos3);
            Ok(ScannedPage { bytes, encoding: PageEncoding::G4, review_image, width, height })
        }
    }
}
```
and near the top: `const COLOR_JPEG_QUALITY: u8 = 80;` (replaces the literal 91; update the doc comment on `render_pdf` that says "q91 bytes" → "the exact encoded bytes").

`page_from_jpeg_bytes` / `pages_from_extracted` (:813-840) become:
```rust
pub fn page_from_encoded(page: EncodedPage) -> Result<ScannedPage, String> {
    let image = decode_page(&page)?;
    if (image.width(), image.height()) != (page.width, page.height) {
        return Err("Wymiary strony nie zgadzają się z jej danymi.".to_owned());
    }
    let review_image = resize_to_fit(&image, STRIP_THUMB_PX, STRIP_THUMB_PX, imageops::FilterType::Lanczos3);
    Ok(ScannedPage {
        width: page.width,
        height: page.height,
        bytes: page.bytes,
        encoding: page.encoding,
        review_image,
    })
}

pub fn pages_from_extracted(pages: Vec<(EncodedPage, u8)>) -> Result<Vec<(ScannedPage, u8)>, String> {
    if pages.is_empty() {
        return Err("Ten PDF nie zawiera stron.".to_owned());
    }
    pages
        .into_iter()
        .enumerate()
        .map(|(index, (page, quarter_turns))| {
            page_from_encoded(page)
                .map(|page| (page, quarter_turns))
                .map_err(|error| format!("Nie można odczytać strony {}: {error}", index + 1))
        })
        .collect()
}
```
For `Jpeg`, `page.width/height` coming from the PDF `/Width /Height` must match the JPEG header — they do for our own files; the check above guards corrupt input.

`render_pdf` (:340-350) — replace the fixed `image_stream` dictionary with:
```rust
        let mut image_dictionary = dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => i64::from(page.width),
            "Height" => i64::from(page.height),
        };
        match page.encoding {
            PageEncoding::Jpeg => {
                image_dictionary.set("ColorSpace", "DeviceRGB");
                image_dictionary.set("BitsPerComponent", 8_i64);
                image_dictionary.set("Filter", "DCTDecode");
            }
            PageEncoding::G4 => {
                image_dictionary.set("ColorSpace", "DeviceGray");
                image_dictionary.set("BitsPerComponent", 1_i64);
                image_dictionary.set("Filter", "CCITTFaxDecode");
                image_dictionary.set(
                    "DecodeParms",
                    dictionary! {
                        "K" => -1_i64,
                        "Columns" => i64::from(page.width),
                        "Rows" => i64::from(page.height),
                        "BlackIs1" => false,
                    },
                );
            }
        }
        let image_stream = Stream::new(image_dictionary, page.bytes.clone()).with_compression(false);
```

`extract_pdf_pages` — change the return type to `Result<Vec<(EncodedPage, u8)>, String>` and replace the `is_jpeg` block (:540-556) + the final `pages.push` with:
```rust
        let Some(encoding) = stream_page_encoding(&document, stream) else {
            return Err("Ten PDF nie pochodzi z tego programu.".to_owned());
        };
        if !is_image {
            return Err("Ten PDF nie pochodzi z tego programu.".to_owned());
        }
        if !page_is_safe_to_edit(&document, page_dictionary, stream, &content.operations) {
            return Err(/* unchanged message */);
        }
        let width = stream.dict.get(b"Width").ok().and_then(pdf_number).map(|v| v as u32).unwrap_or(0);
        let height = stream.dict.get(b"Height").ok().and_then(pdf_number).map(|v| v as u32).unwrap_or(0);
        pages.push((
            EncodedPage { bytes: stream.content.clone(), encoding, width, height },
            quarter_turns,
        ));
```
and add the helper next to `page_quarter_turns`:
```rust
/// `Some(encoding)` when the image stream is exactly one of the two layouts this
/// app writes; a filter chain, G3, inverted polarity or mismatched
/// Columns/Rows is foreign.
fn stream_page_encoding(document: &lopdf::Document, stream: &lopdf::Stream) -> Option<PageEncoding> {
    let filter: &[u8] = match stream.dict.get(b"Filter") {
        Ok(lopdf::Object::Name(name)) => name,
        Ok(lopdf::Object::Array(filters)) if filters.len() == 1 => match filters.first() {
            Some(lopdf::Object::Name(name)) => name,
            _ => return None,
        },
        _ => return None,
    };
    let bpc = stream.dict.get(b"BitsPerComponent").ok().and_then(pdf_number);
    let colorspace = match stream.dict.get(b"ColorSpace") {
        Ok(lopdf::Object::Name(name)) => name.as_slice(),
        _ => return None,
    };
    match filter {
        b"DCTDecode" => (colorspace == b"DeviceRGB" && bpc == Some(8.0)).then_some(PageEncoding::Jpeg),
        b"CCITTFaxDecode" => {
            if colorspace != b"DeviceGray" || bpc != Some(1.0) {
                return None;
            }
            let parms = match stream.dict.get(b"DecodeParms") {
                Ok(lopdf::Object::Dictionary(parms)) => parms,
                Ok(lopdf::Object::Reference(id)) => document.get_object(*id).ok()?.as_dict().ok()?,
                _ => return None,
            };
            let number = |key: &[u8]| parms.get(key).ok().and_then(pdf_number);
            if number(b"K") != Some(-1.0) {
                return None;
            }
            if number(b"Columns") != stream.dict.get(b"Width").ok().and_then(pdf_number)
                || number(b"Rows") != stream.dict.get(b"Height").ok().and_then(pdf_number)
            {
                return None;
            }
            match parms.get(b"BlackIs1") {
                Err(_) | Ok(lopdf::Object::Boolean(false)) => {}
                _ => return None,
            }
            if parms.has(b"EncodedByteAlign") || parms.has(b"EndOfBlock") || parms.has(b"EndOfLine") {
                return None;
            }
            Some(PageEncoding::G4)
        }
        _ => None,
    }
}
```
Also remove the now-unused `is_jpeg` variable and the "lone DCTDecode" comment; `page_is_safe_to_edit` stays as is (dimension check applies to both).

Add the ignored real-frame probe from Task 2 Step 5 into `src/bilevel.rs` tests now (it compiles from here on).

- [ ] **Step 4: Update `pipeline.rs`**

```rust
use crate::document::{ColorMode, CropPoint, ScannedPage, detect_document_corners, process_page_with};
…
enum Job {
    New { id: u64, frame: Arc<RgbImage>, mode: ColorMode },
    Reprocess { id: u64, frame: Arc<RgbImage>, corners: [CropPoint; 4], mode: ColorMode },
    #[cfg(test)]
    TestPanic,
}
…
    pub fn try_submit(&self, id: u64, frame: Arc<RgbImage>, mode: ColorMode) -> bool {
        self.jobs.as_ref().is_some_and(|jobs| jobs.try_send(Job::New { id, frame, mode }).is_ok())
    }

    pub fn submit_reprocess(&self, id: u64, frame: Arc<RgbImage>, corners: [CropPoint; 4], mode: ColorMode) -> bool {
        self.jobs.as_ref().is_some_and(|jobs| jobs.try_send(Job::Reprocess { id, frame, corners, mode }).is_ok())
    }
…
        Job::New { id, frame, mode } => {
            let corners = detect_document_corners(frame);
            … (original_jpeg unchanged) …
            match process_page_with(frame, corners, true, *mode) { … }
        }
        Job::Reprocess { id, frame, corners, mode } => match process_page_with(frame, *corners, false, *mode) { … }
```
Tests: `try_submit(7, …, ColorMode::Color)` / `ColorMode::BlackWhite` and add:
```rust
    #[test]
    fn black_white_mode_yields_g4_pages_and_color_yields_jpeg() {
        use crate::document::PageEncoding;
        let pipeline = ProcessingPipeline::start();
        assert!(pipeline.try_submit(1, Arc::new(white_document_frame(400, 300)), ColorMode::BlackWhite));
        assert!(pipeline.try_submit(2, Arc::new(white_document_frame(400, 300)), ColorMode::Color));
        let events = collect_events(&pipeline, 2, Duration::from_secs(120));
        let mut seen = Vec::new();
        for event in events {
            if let PipelineEvent::PageReady { id, page, .. } = event { seen.push((id, page.encoding, page.bytes.len())); }
        }
        seen.sort();
        assert_eq!(seen[0].1, PageEncoding::G4);
        assert_eq!(seen[1].1, PageEncoding::Jpeg);
        assert!(seen[0].2 < seen[1].2 / 10, "G4 {} B vs JPEG {} B", seen[0].2, seen[1].2);
    }
```

- [ ] **Step 5: Mechanical call-site updates so the crate compiles (behaviour unchanged: app passes `ColorMode::Color` until Task 6)**

`src/app.rs`:
- import: `use crate::document::{ColorMode, CropPoint, EncodedPage, ScannedPage, extract_pdf_pages, page_from_encoded, pages_from_extracted, …}` (drop `page_from_jpeg_bytes`).
- `:673` `pipeline.try_submit(id, frame, ColorMode::Color)`; `:1340` `pipeline.submit_reprocess(entry.id, Arc::new(editor.original), editor.corners, ColorMode::Color)`.
- every `page.jpeg` / `data.page.jpeg` / `recovered_page.jpeg` → `.bytes` (lines 725, 726, 768, 964, 1240, 1438, 2392) — `session_write_page` keeps taking `&[u8]` in this task (Task 4 changes it), so pass `&page.bytes`.
- `:1247-1249`: `processed_jpeg.ok_or_else(…).and_then(|bytes| page_from_encoded(EncodedPage { bytes, encoding: PageEncoding::Jpeg, width: 0, height: 0 }))` would fail the dimension check — instead, **for this task only**, decode dims first:
  ```rust
  let recovered_page = processed_jpeg
      .ok_or_else(|| "Brak przetworzonego obrazu strony.".to_owned())
      .and_then(|bytes| {
          let (width, height) = image::load_from_memory(&bytes)
              .map(|image| (image.width(), image.height()))
              .map_err(|error| format!("Nie można odczytać zeskanowanej strony: {error}"))?;
          page_from_encoded(EncodedPage { bytes, encoding: PageEncoding::Jpeg, width, height })
      });
  ```
  (Task 4 replaces this with the session's own `EncodedPage`.) Add `PageEncoding` to the import.
- `:2392` review: `self.review_viewport.ensure_page(context, PageTextureKey{…}, &data.page.encoded(), Some(data.texture.clone()))` — drop the `Vec2::new(width,height)` argument.

`src/review_viewport.rs`:
- `DecodeJob { key, page: EncodedPage, max_texture_side }`; `ensure_page(&mut self, context, key, page: &EncodedPage, placeholder)` with `let page_px = Vec2::new(page.width as f32, page.height as f32);` replacing the parameter; send `page: page.clone()`.
- `decode_full_texture(context, key, page: &EncodedPage, max_texture_side)`: replace `image::load_from_memory(jpeg)…to_rgb8()` with `crate::document::decode_page(page).map_err(|error| format!("Nie można otworzyć pełnego podglądu: {error}"))?`.
- import `use crate::document::EncodedPage;`.

`src/sync.rs` / `spawn_scan_upload`: unchanged in this task (still receives `Vec<u8>` JPEG bytes = `page.bytes.clone()`; semantically wrong for G4 but the app still only produces JPEG until Task 6; fixed in Task 5).

- [ ] **Step 6: Run the whole suite**

Run: `$SCRATCH/gm-push.sh src/document.rs src/pipeline.rs src/app.rs src/review_viewport.rs && $SCRATCH/gm-test.sh`
Expected: all pass — 94 old + 4 codec + 4 binarize + 7 document + 1 pipeline = **110 passed** (ignored tests not counted). Also run `ssh gm "cd /d D:\scan-app && %USERPROFILE%\.cargo\bin\cargo.exe clippy --release 2>&1 | tail -20"` → no warnings.

- [ ] **Step 7: Commit**

Run: `$SCRATCH/gm-commit.sh "feat(document): PageEncoding/EncodedPage, G4 pipeline path, CCITTFaxDecode PDF write and read"`

---

### Task 4: Session format 2 (`session.rs`, `app.rs`)

**Files:**
- Modify: `src/session.rs` (`RecoveredPage` :20-27, `PageMetadata` :50-57, paths :80-117, `write_page` :156-188, `load_from_manifest` :218-256, `salvage_orphan_pages` :261-328, `SessionCommand`/`SessionWorker::write_page` :348-440, `parse_session_file_name` :490-502, tests)
- Modify: `src/app.rs` (`session_write_page` :502-515 and its 6 callers; restore :1238-1250)

**Interfaces:**
- Produces: `SessionStore::write_page(&self, id, page: &EncodedPage, original_jpeg: &[u8], corners, quarter_turns) -> Result<(), String>`; `SessionWorker::write_page(&self, id, page: &EncodedPage, original_jpeg, corners, quarter_turns)`; `RecoveredPage { id, page: Option<EncodedPage>, original_jpeg, corners, quarter_turns }`.
- Session files: page bytes at `{id}[.r{rev}].jpg` (Jpeg) or `{id}[.r{rev}].g4` (G4); metadata `{id}[.r{rev}].crop.ron` with `format: 2, encoding, width, height`.

- [ ] **Step 1: Update existing tests to the new API and add the new ones**

In `mod tests` of `src/session.rs`, add a helper and use it everywhere a page is written:
```rust
    fn jpeg(bytes: &[u8]) -> EncodedPage {
        EncodedPage { bytes: bytes.to_vec(), encoding: PageEncoding::Jpeg, width: 80, height: 120 }
    }
    fn g4(bytes: &[u8]) -> EncodedPage {
        EncodedPage { bytes: bytes.to_vec(), encoding: PageEncoding::G4, width: 2480, height: 3508 }
    }
```
- `store.write_page(5, b"piata-strona", …)` → `store.write_page(5, &jpeg(b"piata-strona"), …)` (all 12 call sites in tests);
- assertions `recovered.pages[0].jpeg.as_deref() == Some(b"…")` → `recovered.pages[0].page.as_ref().map(|p| p.bytes.as_slice()) == Some(b"…".as_slice())`; `recovered.pages[0].jpeg == None` → `.page.is_none()`.

New tests:
```rust
    #[test]
    fn g4_page_round_trips_with_encoding_and_dimensions() {
        let store = test_store("g4");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store.write_page(2, &g4(b"g4-bajty"), b"oryginal", corners(), 1).expect("write");
        assert!(store.revision_paths(2, 1).0.with_extension("g4").exists());
        assert!(!store.revision_paths(2, 1).0.exists(), "no .jpg for a G4 page");
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages[0].page, Some(g4(b"g4-bajty")));
        assert_eq!(recovered.pages[0].quarter_turns, 1);
        store.clear().expect("clear");
    }

    #[test]
    fn format_one_metadata_still_restores_as_jpeg() {
        let store = test_store("format1");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        fs::write(store.page_path(4), b"jpeg-bajty").expect("page");
        fs::write(
            store.metadata_path(4),
            "(corners:((x:0.1,y:0.2),(x:0.9,y:0.2),(x:0.9,y:0.8),(x:0.1,y:0.8)),quarter_turns:2,format:1)",
        )
        .expect("metadata");
        let mut manifest = store.read_manifest().expect("manifest");
        manifest.page_ids.push(4);
        store.write_manifest(&manifest).expect("manifest update");
        let recovered = store.load_existing().expect("session");
        let page = recovered.pages[0].page.as_ref().expect("page");
        assert_eq!(page.encoding, PageEncoding::Jpeg);
        assert_eq!(page.bytes, b"jpeg-bajty");
        assert_eq!((page.width, page.height), (0, 0), "legacy dims are unknown here; the app reads them from the JPEG");
        assert_eq!(recovered.pages[0].quarter_turns, 2);
        store.clear().expect("clear");
    }

    #[test]
    fn orphan_g4_without_metadata_is_skipped_not_guessed() {
        let store = test_store("orphan-g4");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store.write_page(3, &g4(b"g4"), b"o3", corners(), 0).expect("write 3");
        store.write_page(5, &jpeg(b"jpg"), b"o5", corners(), 0).expect("write 5");
        let (_, _, metadata_3) = store.revision_paths(3, 1);
        fs::remove_file(metadata_3).expect("drop metadata of 3");
        fs::write(store.manifest_path(), "###nie-ron###").expect("corrupt manifest");
        let recovered = store.load_existing().expect("salvage");
        let ids: Vec<u64> = recovered.pages.iter().map(|page| page.id).collect();
        assert_eq!(ids, vec![3, 5]);
        assert!(recovered.pages[0].page.is_none(), "G4 bytes without dims are unusable");
        assert_eq!(recovered.pages[0].original_jpeg.as_deref(), Some(b"o3".as_slice()));
        assert_eq!(recovered.pages[1].page, Some(jpeg(b"jpg")));
        store.clear().expect("clear");
    }
```
(`jpeg()` helper uses 80×120 but the format-1 test asserts `(0,0)` — legacy metadata has no dims, so `load_from_manifest` must report `0,0` for format < 2 and let the app fill them; see Step 3.)

- [ ] **Step 2: Run tests to verify they fail**

Run: `$SCRATCH/gm-push.sh src/session.rs && $SCRATCH/gm-test.sh session`
Expected: compile errors (`write_page` signature, `page` field).

- [ ] **Step 3: Implement**

`src/session.rs`:
```rust
use crate::document::{CropPoint, EncodedPage, PageEncoding};
…
pub struct RecoveredPage {
    pub id: u64,
    /// `width/height` are 0 for format-0/1 sessions (JPEG only); the app
    /// reads them from the JPEG header in that case.
    pub page: Option<EncodedPage>,
    pub original_jpeg: Option<Vec<u8>>,
    pub corners: Option<[CropPoint; 4]>,
    pub quarter_turns: u8,
}
…
/// Format 0 (legacy) stored the page JPEG already rotated; format 1 keeps the
/// JPEG unrotated with `quarter_turns` as display metadata; format 2 adds the
/// page `encoding` (the file is `.g4` for G4) and its pixel dimensions.
#[derive(Serialize, Deserialize)]
struct PageMetadata {
    corners: [CropPoint; 4],
    #[serde(default)]
    quarter_turns: u8,
    #[serde(default)]
    format: u8,
    #[serde(default = "default_encoding")]
    encoding: PageEncoding,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

fn default_encoding() -> PageEncoding {
    PageEncoding::Jpeg
}

const PAGE_METADATA_FORMAT: u8 = 2;

fn page_extension(encoding: PageEncoding) -> &'static str {
    match encoding {
        PageEncoding::Jpeg => "jpg",
        PageEncoding::G4 => "g4",
    }
}
```
Paths: keep `page_path`/`revision_paths` returning the `.jpg` path (it is the *Jpeg* page path); add
```rust
    /// The page file for an encoding: `.jpg` for JPEG, `.g4` for G4 — same stem.
    fn page_path_for(&self, id: u64, revision: Option<u64>, encoding: PageEncoding) -> PathBuf {
        self.paths_for(id, revision).0.with_extension(page_extension(encoding))
    }

    /// Reads whichever page file exists (JPEG first, then G4) and tells which.
    fn read_page_file(&self, id: u64, revision: Option<u64>) -> Option<(Vec<u8>, PageEncoding)> {
        for encoding in [PageEncoding::Jpeg, PageEncoding::G4] {
            if let Ok(bytes) = fs::read(self.page_path_for(id, revision, encoding)) {
                return Some((bytes, encoding));
            }
        }
        None
    }
```
`remove_revision_files`: also `let _ = fs::remove_file(page.with_extension("g4"));`.

`write_page`:
```rust
    pub fn write_page(
        &self,
        id: u64,
        page: &EncodedPage,
        original_jpeg: &[u8],
        corners: [CropPoint; 4],
        quarter_turns: u8,
    ) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        let previous_revision = manifest.page_revisions.get(&id).copied();
        let revision = previous_revision.unwrap_or(0) + 1;
        let (_, original_path, metadata_path) = self.revision_paths(id, revision);
        let page_path = self.page_path_for(id, Some(revision), page.encoding);
        crate::atomic_file::write(&page_path, page.bytes.clone()).map_err(io_error)?;
        crate::atomic_file::write(&original_path, original_jpeg).map_err(io_error)?;
        let metadata = ron::to_string(&PageMetadata {
            corners,
            quarter_turns: quarter_turns % 4,
            format: PAGE_METADATA_FORMAT,
            encoding: page.encoding,
            width: page.width,
            height: page.height,
        })
        .map_err(|error| error.to_string())?;
        … (rest unchanged)
```
(check `atomic_file::write`'s signature — it currently takes the bytes by value/`impl AsRef<[u8]>`; pass whatever the existing call passed, i.e. `&page.bytes` if it accepted `&[u8]` before.)

`load_from_manifest` and `salvage_orphan_pages` — replace the `jpeg = fs::read(page_path).ok()` logic with:
```rust
            let page_file = self.read_page_file(*id, revision);
            let original_jpeg = fs::read(original_path).ok();
            if page_file.is_none() && original_jpeg.is_none() {
                skipped_pages += 1;
                continue;
            }
            let metadata = fs::read_to_string(metadata_path)
                .ok()
                .and_then(|contents| ron::from_str::<PageMetadata>(&contents).ok());
            let page = page_file.and_then(|(bytes, file_encoding)| match file_encoding {
                PageEncoding::Jpeg => Some(EncodedPage {
                    bytes,
                    encoding: PageEncoding::Jpeg,
                    width: metadata.as_ref().filter(|m| m.format >= 2).map_or(0, |m| m.width),
                    height: metadata.as_ref().filter(|m| m.format >= 2).map_or(0, |m| m.height),
                }),
                // A G4 stream is meaningless without its dimensions.
                PageEncoding::G4 => metadata
                    .as_ref()
                    .filter(|m| m.format >= 2 && m.encoding == PageEncoding::G4 && m.width > 0 && m.height > 0)
                    .map(|m| EncodedPage { bytes, encoding: PageEncoding::G4, width: m.width, height: m.height }),
            });
            pages.push(RecoveredPage { id: *id, page, original_jpeg, corners: …, quarter_turns: … });
```
(in `salvage_orphan_pages` use `id` not `*id`, and the `continue` instead of `skipped_pages`, exactly as the existing code structure does). `parse_session_file_name`: add `else if let Some(head) = name.strip_suffix(".g4") { (head, SessionFileKind::Page) }` before the `.jpg` fallback. `SessionCommand::WritePage { id, page: EncodedPage, original_jpeg, corners, quarter_turns }` and `SessionWorker::write_page(&self, id, page: &EncodedPage, original_jpeg: &[u8], corners, quarter_turns)` → `page: page.clone()`.

`src/app.rs`:
- `session_write_page(&mut self, id, page: &EncodedPage, original_jpeg: &[u8], corners, quarter_turns)`; callers pass `&page.encoded()` / `&data.page.encoded()` (lines 725, 780 — there `persisted` tuple holds `page.encoded()` instead of `page.jpeg.clone()`, 964-970, 1438).
- restore (:1238-1250):
  ```rust
            let recovered_page = recovered_page
                .page
                .ok_or_else(|| "Brak przetworzonego obrazu strony.".to_owned())
                .and_then(|mut encoded| {
                    if encoded.width == 0 || encoded.height == 0 {
                        // Format 0/1 sessions: dimensions live only in the JPEG header.
                        let image = image::load_from_memory(&encoded.bytes)
                            .map_err(|error| format!("Nie można odczytać zeskanowanej strony: {error}"))?;
                        encoded.width = image.width();
                        encoded.height = image.height();
                    }
                    page_from_encoded(encoded)
                });
  ```
  (remove the interim Task-3 code there; `let processed_jpeg = recovered_page.jpeg;` goes away.)

- [ ] **Step 4: Run the suite**

Run: `$SCRATCH/gm-push.sh src/session.rs src/app.rs && $SCRATCH/gm-test.sh`
Expected: **113 passed** (110 + 3), clippy clean.

- [ ] **Step 5: Commit**

Run: `$SCRATCH/gm-commit.sh "feat(session): format 2 metadata with page encoding and dimensions, .g4 page files"`

---

### Task 5: Cloud sync payload for bilevel pages (`sync.rs`, `app.rs`)

**Files:**
- Modify: `src/sync.rs` (`spawn_upload` :22-35, `upload_scan` :37-67, new `upload_payload`, tests)
- Modify: `src/app.rs:536-558, 726` (`spawn_scan_upload(id, page: EncodedPage)`)

**Interfaces:**
- Produces: `pub fn upload_payload(page: &EncodedPage) -> Result<(Vec<u8>, &'static str /*mime*/, &'static str /*extension*/), String>`; `pub fn spawn_upload(backend_url, salon_id, api_key, page_id, page: EncodedPage, tx)` (file name is built inside as `scan-{page_id}.{ext}`).

- [ ] **Step 1: Write the failing test**

Add to `src/sync.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{EncodedPage, PageEncoding};

    #[test]
    fn jpeg_pages_upload_as_is() {
        let page = EncodedPage { bytes: vec![0xFF, 0xD8, 0xFF, 0xD9], encoding: PageEncoding::Jpeg, width: 1, height: 1 };
        let (bytes, mime, ext) = upload_payload(&page).expect("payload");
        assert_eq!(bytes, page.bytes);
        assert_eq!((mime, ext), ("image/jpeg", "jpg"));
    }

    #[test]
    fn g4_pages_upload_as_png_the_backend_can_decode() {
        let mut image = image::GrayImage::from_pixel(64, 32, image::Luma([255]));
        for x in 10..20 { image.put_pixel(x, 5, image::Luma([0])); }
        let page = EncodedPage { bytes: crate::bilevel::encode_g4(&image), encoding: PageEncoding::G4, width: 64, height: 32 };
        let (bytes, mime, ext) = upload_payload(&page).expect("payload");
        assert_eq!((mime, ext), ("image/png", "png"));
        let decoded = image::load_from_memory(&bytes).expect("png").to_luma8();
        assert_eq!(decoded.dimensions(), (64, 32));
        assert_eq!(decoded.get_pixel(15, 5), &image::Luma([0]));
        assert_eq!(decoded.get_pixel(40, 20), &image::Luma([255]));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `$SCRATCH/gm-push.sh src/sync.rs && $SCRATCH/gm-test.sh sync`
Expected: compile error `cannot find function upload_payload`.

- [ ] **Step 3: Implement**

`src/sync.rs`:
```rust
use crate::document::{EncodedPage, PageEncoding};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
…
/// Bytes + MIME + extension the backend accepts. The backend (sharp) only
/// takes jpeg/png/webp, so a G4 page is shipped as an 8-bit grayscale PNG
/// (two-valued, so deflate keeps it ~200 KB). `image` 0.25 cannot write 1-bit PNG.
pub fn upload_payload(page: &EncodedPage) -> Result<(Vec<u8>, &'static str, &'static str), String> {
    match page.encoding {
        PageEncoding::Jpeg => Ok((page.bytes.clone(), "image/jpeg", "jpg")),
        PageEncoding::G4 => {
            let gray = crate::bilevel::decode_g4(&page.bytes, page.width, page.height)?;
            let mut png = Vec::new();
            PngEncoder::new(&mut png)
                .write_image(gray.as_raw(), page.width, page.height, ExtendedColorType::L8)
                .map_err(|error| format!("Nie można przygotować strony do wysyłki: {error}"))?;
            Ok((png, "image/png", "png"))
        }
    }
}

pub fn spawn_upload(backend_url: String, salon_id: String, api_key: String, page_id: u64, page: EncodedPage, tx: Sender<SyncOutcome>) {
    std::thread::spawn(move || {
        let result = upload_payload(&page).and_then(|(bytes, mime, ext)| {
            upload_scan(&backend_url, &salon_id, &api_key, &format!("scan-{page_id}.{ext}"), mime, bytes)
        });
        let _ = tx.send(SyncOutcome { page_id, result });
    });
}

fn upload_scan(backend_url: &str, salon_id: &str, api_key: &str, file_name: &str, mime: &str, bytes: Vec<u8>) -> Result<String, String> {
    … `multipart::Part::bytes(bytes).file_name(file_name.to_owned()).mime_str(mime)` … (rest unchanged)
}
```
`src/app.rs`: `fn spawn_scan_upload(&mut self, id: u64, page: EncodedPage)` → `spawn_upload(backend_url.to_owned(), salon_id.to_owned(), api_key, id, page, self.sync_tx.clone())` (drop the `format!("scan-{id}.jpg")` argument); caller `:726` → `self.spawn_scan_upload(id, page.encoded());`. Update the spec sentence in `docs/superpowers/specs/2026-08-20-bilevel-g4-compact-pdf-design.md` §3.8: "1-bit PNG (~115 KB)" → "8-bit grayscale PNG (~200 KB; image 0.25 cannot write L1)".

- [ ] **Step 4: Run the suite**

Run: `$SCRATCH/gm-push.sh src/sync.rs src/app.rs docs/superpowers/specs/2026-08-20-bilevel-g4-compact-pdf-design.md && $SCRATCH/gm-test.sh`
Expected: **115 passed**.

- [ ] **Step 5: Commit**

Run: `$SCRATCH/gm-commit.sh "feat(sync): upload bilevel pages as grayscale PNG"`

---

### Task 6: „Tryb koloru” setting, default black-and-white, page-size hint (`storage.rs`, `app.rs`)

**Files:**
- Modify: `src/storage.rs:31-46` (`Settings.color_mode`), tests :720-730
- Modify: `src/app.rs` (settings modal after the „Folder biblioteki" block and before „Synchronizacja z chmurą" :2885; `try_submit`/`submit_reprocess` calls :673/:1340; film-strip tile :2190; review inspector :2501)

**Interfaces:**
- Consumes: `ColorMode` (document.rs).
- Produces: `Settings.color_mode: Option<ColorMode>` (`None` ⇒ BlackWhite); `App::color_mode(&self) -> ColorMode`.

- [ ] **Step 1: Write the failing test (storage)**

Append to `mod tests` in `src/storage.rs`:
```rust
    #[test]
    fn color_mode_defaults_to_black_white_and_round_trips() {
        use crate::document::ColorMode;
        let legacy: Settings = ron::from_str("(library_root:None,last_folder:None)").unwrap();
        assert_eq!(legacy.color_mode, None);
        assert_eq!(legacy.color_mode.unwrap_or_default(), ColorMode::BlackWhite);
        let mut settings = Settings::default();
        settings.color_mode = Some(ColorMode::Color);
        let text = ron::to_string(&settings).unwrap();
        let back: Settings = ron::from_str(&text).unwrap();
        assert_eq!(back.color_mode, Some(ColorMode::Color));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `$SCRATCH/gm-push.sh src/storage.rs && $SCRATCH/gm-test.sh storage`
Expected: compile error `no field color_mode`.

- [ ] **Step 3: Implement**

`src/storage.rs` — `use crate::document::ColorMode;` and in `Settings`:
```rust
    /// `None` means the default, black-and-white (G4) pages.
    #[serde(default)]
    pub color_mode: Option<ColorMode>,
```
`src/app.rs`:
```rust
    fn color_mode(&self) -> ColorMode {
        self.settings.color_mode.unwrap_or_default()
    }
```
- `:673` `pipeline.try_submit(id, frame, self.color_mode())` — note the borrow: take `let mode = self.color_mode();` before `let Some(pipeline) = &self.pipeline`.
- `:1340` `pipeline.submit_reprocess(entry.id, Arc::new(editor.original), editor.corners, mode)` with `let mode = self.color_mode();` computed before the slot borrow.
- Settings modal — insert right before `ui.add_space(14.0); ui.label(RichText::new("Synchronizacja z chmurą").strong());`:
```rust
                ui.add_space(14.0);
                ui.label(RichText::new("Tryb koloru").strong());
                ui.label(
                    RichText::new(
                        "Czarno-biały daje strony ~66 KB zamiast ~2 MB bez utraty rozdzielczości. \
                         Kolor tylko dla dokumentów z pieczęciami lub kolorowym papierem.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
                ui.add_space(6.0);
                let mut mode = self.color_mode();
                let mode_changed = egui::ComboBox::from_id_salt("tryb-koloru")
                    .width(260.0)
                    .selected_text(match mode {
                        ColorMode::BlackWhite => "Czarno-biały (domyślnie)",
                        ColorMode::Color => "Kolor",
                    })
                    .show_ui(ui, |ui| {
                        let a = ui.selectable_value(&mut mode, ColorMode::BlackWhite, "Czarno-biały (domyślnie)").changed();
                        let b = ui.selectable_value(&mut mode, ColorMode::Color, "Kolor").changed();
                        a || b
                    })
                    .inner
                    .unwrap_or(false);
                if mode_changed {
                    self.settings.color_mode = Some(mode);
                    let _ = save_settings(&self.settings);
                }
                ui.label(
                    RichText::new("Zmiana dotyczy kolejnych skanowanych stron.")
                        .small()
                        .color(Color32::GRAY),
                );
```
- Film strip tile (:2190, `PageSlot::Ready` branch): `ui.label(format!("{}", index + 1)).on_hover_text(format!("{} KB", data.page.bytes.len() / 1024));`
- Review inspector (:2501): below the `Strona {}` heading add `ui.label(RichText::new(page_size_label(&data.page)).small().color(Color32::GRAY));` where `data` is the `PageSlot::Ready` data already in scope there (check the surrounding `match` — if only `index` is in scope, look the slot up: `if let Some(SlotEntry { slot: PageSlot::Ready(data), .. }) = self.slots.get(index)`), with
```rust
fn page_size_label(page: &ScannedPage) -> String {
    let kb = page.bytes.len() / 1024;
    let mode = match page.encoding {
        crate::document::PageEncoding::G4 => "czarno-biała",
        crate::document::PageEncoding::Jpeg => "kolor",
    };
    format!("{kb} KB · {mode}")
}
```
added next to `fn strip_texture` (:3547).

- [ ] **Step 4: Run the suite + clippy**

Run: `$SCRATCH/gm-push.sh src/storage.rs src/app.rs && $SCRATCH/gm-test.sh && ssh gm "cd /d D:\scan-app && %USERPROFILE%\.cargo\bin\cargo.exe clippy --release 2>&1 | tail -15"`
Expected: **116 passed**, clippy clean.

- [ ] **Step 5: Commit**

Run: `$SCRATCH/gm-commit.sh "feat(settings): Tryb koloru (default czarno-biały), page size hints"`

---

### Task 7: Release build on gm, real-frame probe, visual check, merge + push

**Files:** none (verification). Uses the real frame captured on klaud (`D:\scan-app` has none; copy one JPEG from the library via `pdfimages` on Netcup — `$SCRATCH/pg-001.jpg` is a processed page, not a raw frame; a raw frame is needed: `ssh klaud-laptop` and look for `%APPDATA%\SkanerDokumentow\Skaner dokumentów\data\sesja\*.original.jpg` **only if no scan session is active** (the app is running — read-only copy of an `.original.jpg` is safe; do not touch `manifest.ron`)).

- [ ] **Step 1: Release build + full test on gm**

Run: `ssh gm "cd /d D:\scan-app && taskkill /im skaner-dokumentow.exe /f >nul 2>&1 & %USERPROFILE%\.cargo\bin\cargo.exe build --release 2>&1 | tail -3 && dir target\release\skaner-dokumentow.exe"` then `$SCRATCH/gm-test.sh`
Expected: exe rebuilt (timestamp now), 116 passed.

- [ ] **Step 2: Real-frame probe**

Copy a raw frame to gm (`scp <frame> gm:D:/scan-test/frame.jpg`) and run:
`ssh gm "cd /d D:\scan-app && set IRISCAN_TEST_FRAME=D:\scan-test\frame.jpg&& %USERPROFILE%\.cargo\bin\cargo.exe test --release bilevel_real_frame -- --ignored --nocapture 2>&1 | tail -8"`
Expected: `bytes=30..150 KB decode<300ms`, test passes. Also write the G4 page to a PDF and inspect it on Netcup: extend the probe temporarily with `std::fs::write("D:\\scan-test\\probe.pdf", crate::document::render_pdf(&[(&page, 0)]).unwrap())` (not committed), `scp gm:D:/scan-test/probe.pdf $SCRATCH/` and run `pdfimages -list $SCRATCH/probe.pdf` → `gray 1 1 ccitt 2480 3508`, `pdftoppm -r 100 -png $SCRATCH/probe.pdf $SCRATCH/probe` and look at the PNG (Read tool) — text crisp, no rim, no speckle.

- [ ] **Step 3: GUI smoke on gm desktop (if the screen is unlocked)**

Launch via the existing schtasks `/it` pattern (see memory `scan-app-conveyor-rework`), open Ustawienia → the „Tryb koloru" combo shows „Czarno-biały (domyślnie)", switch to Kolor and back — `ustawienia.ron` gains `color_mode:Some(Color)`/`Some(BlackWhite)`. Open an old library PDF from `D:\scan-test` for edit → pages show; save → still opens.

- [ ] **Step 4: Merge to main and push**

Run:
```bash
ssh gm "cd /d D:\scan-app && git checkout main && git merge --ff-only feat/bilevel-g4-20260820 && git log --oneline -8 && git push origin main feat/bilevel-g4-20260820 2>&1 | tail -3"
```
Expected: fast-forward, push OK (GCM credential cached since 06/08; if the push prompts, fall back to the interactive window via schtasks as in memory). Then `cd $SCRATCH/scan-app && git fetch origin && git status -sb`.

- [ ] **Step 5: Deploy to klaud-laptop (only when staff is idle — check `Get-Process skaner-dokumentow` start time and the newest PDF mtime) and run the manual acceptance**

```bash
ssh klaud-laptop "powershell -NoProfile -Command \"Get-Process skaner-dokumentow -EA SilentlyContinue | select Id,StartTime; Get-ChildItem 'C:\Users\klaud\Contacts\Documents\Zeskanowane dokumenty' -Recurse -Filter *.pdf | sort LastWriteTime -desc | select -first 1 LastWriteTime\""
ssh klaud-laptop "taskkill /im skaner-dokumentow.exe /f & cd /d D:\scan-app && C:\tools\mingit\cmd\git.exe pull --ff-only && %USERPROFILE%\.cargo\bin\cargo.exe build --release 2>&1 | tail -3"
```
Manual acceptance (Paul / staff, desktop): scan 5 pages → PDF < 0.5 MB; scan ≥ 40 pages → < 5 MB; open in Edge and Acrobat, zoom 400 % on small print and a signature; reopen in app, rotate one page, save; kill mid-scan → restore dialog lists pages; switch to Kolor, scan one stamped page, back to Czarno-biały. Record sizes in the memory file.

## Self-review notes

- Spec §3.2 border rule refined in Task 2 to "thin rule" (≥ 60 % long, ≤ 0.4 % thick) — the bare "> 60 % span" rule would keep the mat rim (it spans 100 % of the height); the spec wording is updated inside Task 2 Step 3.
- Spec §3.5 "reprocess warps the decoded page raster" — the editor refuses pages without an original (`Ta strona nie ma zapisanego oryginału`), so that path does not exist; nothing to implement. Note it in the spec in Task 3's commit (§3.5 last paragraph → "pages loaded from a PDF have no original frame, so the editor stays disabled for them (existing behaviour)").
- Spec §3.8 PNG depth corrected in Task 5.
