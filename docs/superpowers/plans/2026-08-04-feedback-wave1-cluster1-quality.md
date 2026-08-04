# Feedback Wave 1 / Cluster 1 — Image Quality: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Scans embed the full 300-DPI page image (no printpdf downsampling) and paper backgrounds come out neutral white regardless of camera color cast.

**Architecture:** Two surgical changes in `src/document.rs`: `save_pdf` passes `PdfSaveOptions { image_optimization: None, .. }` (verified against vendored printpdf 0.12.5: `Default::default()` sets `image_optimization: Some(ImageOptimizationOptions::default())` whose `max_image_size` recompresses pages to ~150 KB / 85 DPI); `enhance_document` switches from one luminance-based scale to independent per-channel percentile normalization.

**Tech Stack:** unchanged (printpdf 0.12.5, image 0.25).

**Spec:** `docs/superpowers/specs/2026-08-04-feedback-wave1-design.md` §3.1–§3.2

## Global Constraints

Same as previous plans: repo of record `gm:D:\scan-app` (main), local editing copy = scratchpad clone, cargo via `%USERPROFILE%\.cargo\bin\cargo.exe`, tests `--release`, SYNC/TEST/BUILD/COMMIT templates unchanged. Deploy target: klaud-laptop `D:\scan-app` via `git pull` + local build — **the running app locks the exe; coordinate the restart with Paul (testers may be mid-stack; session recovery protects pages but the restart still interrupts).**

---

### Task 1: Disable printpdf image optimization

**Files:**
- Modify: `src/document.rs` (`save_pdf` line ~301 and test module)

**Interfaces:**
- Consumes: existing `save_pdf(&Path, &[&ScannedPage])`, `page_from_image`.
- Produces: unchanged signature; PDFs now embed original JPEG bytes.

- [ ] **Step 1: Write the failing test** (document.rs test module)

```rust
    #[test]
    fn saved_pdf_keeps_full_resolution() {
        let image = RgbImage::from_pixel(1000, 1414, Rgb([245, 245, 245]));
        let page = page_from_image(image).expect("strona testowa");
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-res-{}.pdf",
            std::process::id()
        ));
        save_pdf(&path, &[&page]).expect("zapis PDF");
        let bytes = std::fs::read(&path).expect("odczyt PDF");
        std::fs::remove_file(&path).expect("usunięcie testowego PDF");
        let start = bytes
            .windows(3)
            .position(|w| w == [0xFF, 0xD8, 0xFF])
            .expect("brak strumienia JPEG w PDF");
        let end = bytes[start..]
            .windows(2)
            .position(|w| w == [0xFF, 0xD9])
            .map(|offset| start + offset + 2)
            .expect("brak końca JPEG");
        let embedded = image::load_from_memory(&bytes[start..end]).expect("dekodowanie JPEG");
        assert_eq!(
            (embedded.width(), embedded.height()),
            (1000, 1414),
            "printpdf zmniejszył osadzony obraz"
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

SYNC + TEST filtered: `... cargo.exe test --release saved_pdf_keeps -- --nocapture` — expected FAIL: embedded dimensions smaller than 1000×1414 (optimizer downsampled).

- [ ] **Step 3: Implement**

In `save_pdf`, replace

```rust
    let bytes = document
        .with_pages(pdf_pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new());
```

with

```rust
    let save_options = PdfSaveOptions {
        image_optimization: None,
        ..PdfSaveOptions::default()
    };
    let bytes = document
        .with_pages(pdf_pages)
        .save(&save_options, &mut Vec::new());
```

- [ ] **Step 4: TEST** — new test passes, whole suite green.

- [ ] **Step 5: Commit** — `fix: embed full-resolution page images in PDF (disable printpdf image optimization)`

---

### Task 2: Per-channel background neutralization

**Files:**
- Modify: `src/document.rs` (`enhance_document` + test module)

**Interfaces:**
- Consumes/Produces: `fn enhance_document(image: &mut RgbImage)` signature unchanged (private, called by `process_page`).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn neutralizes_blue_color_cast() {
        let mut image = RgbImage::from_pixel(400, 400, Rgb([190, 200, 230]));
        for y in 150..250 {
            for x in 150..250 {
                image.put_pixel(x, y, Rgb([40, 45, 70]));
            }
        }
        enhance_document(&mut image);
        let corner = image.get_pixel(10, 10);
        assert!(
            corner[0] >= 240 && corner[1] >= 240 && corner[2] >= 240,
            "tło nie zostało rozjaśnione: {:?}",
            corner
        );
        let max_channel_gap = corner[0]
            .abs_diff(corner[1])
            .max(corner[1].abs_diff(corner[2]))
            .max(corner[0].abs_diff(corner[2]));
        assert!(
            max_channel_gap <= 6,
            "tło nie jest neutralne: {:?}",
            corner
        );
    }
```

- [ ] **Step 2: Run to verify it fails** — filtered `neutralizes_blue` — expected FAIL on the neutrality assert (current code preserves the cast: single scale for all channels).

- [ ] **Step 3: Implement** — replace `enhance_document` body:

```rust
fn enhance_document(image: &mut RgbImage) {
    let mut histograms = [[0_u64; 256]; 3];
    for pixel in image.pixels().step_by(8) {
        for channel in 0..3 {
            histograms[channel][pixel[channel] as usize] += 1;
        }
    }
    let sample_count: u64 = histograms[0].iter().sum();
    if sample_count == 0 {
        return;
    }
    let mut lows = [0_usize; 3];
    let mut highs = [0_usize; 3];
    for channel in 0..3 {
        lows[channel] = percentile(&histograms[channel], sample_count / 200);
        highs[channel] = percentile(&histograms[channel], sample_count * 199 / 200);
    }
    if (0..3).any(|channel| highs[channel] <= lows[channel] + 60) {
        return;
    }
    let scales: [f32; 3] = std::array::from_fn(|channel| {
        245.0 / (highs[channel] - lows[channel]) as f32
    });
    for pixel in image.pixels_mut() {
        for channel in 0..3 {
            let adjusted =
                (pixel[channel] as i32 - lows[channel] as i32) as f32 * scales[channel] + 5.0;
            pixel[channel] = adjusted.round().clamp(0.0, 255.0) as u8;
        }
    }
}
```

(`percentile` helper unchanged. Guard mirrors the old `high <= low + 60` per channel —
low-contrast frames stay untouched, same as before.)

- [ ] **Step 4: TEST** — new test passes; whole suite green (`detects_a_perspective_document` and pipeline tests unaffected: they don't assert colors).

- [ ] **Step 5: Commit** — `fix: neutralize color cast via per-channel normalization`

---

### Task 3: Deploy to klaud-laptop

- [ ] **Step 1:** Full TEST + BUILD green on gm.
- [ ] **Step 2:** Push `main` from gm (interactive window pattern — Paul clicks).
- [ ] **Step 3:** Ask Paul for a restart window (testers!). Then on the laptop:
  `taskkill /im skaner-dokumentow.exe /f`, `git pull` (MinGit full path), `cargo build --release`, relaunch via schtasks `/it`.
- [ ] **Step 4:** Acceptance with the next real scan: `pdfimages -list` on the new PDF shows 2480×3508 @ ~300 ppi and page background is neutral white.

## Self-Review Notes

- Spec §3.1 → Task 1 (exact field `image_optimization: None`, verified against vendored source: `Default` sets `Some(ImageOptimizationOptions::default())` with `max_image_size` default). §3.2 → Task 2 (percentiles 0.5/99.5 as sampled `/200` and `*199/200`, guard 60, targets 5/250 — matches spec within the existing code idiom of 5.0 offset + 245 scale). §3.3 (camera probe) is a diagnostic run, not app code — tracked outside this plan.
- Type check: `histograms` indexing via `pixel[channel]` (image::Rgb Index<usize>) ✓; `std::array::from_fn` stable ✓.
