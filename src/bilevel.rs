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

/// Encodes a bilevel image as a raw CCITT Group 4 (T.6) stream — exactly the
/// bytes a PDF `CCITTFaxDecode` filter with `/K -1 /BlackIs1 false` expects.
/// Any pixel darker than 128 is ink.
pub fn encode_g4(image: &GrayImage) -> Vec<u8> {
    let width = image.width();
    let mut encoder = Encoder::new(VecWriter::with_capacity(
        width as usize * image.height() as usize / 16,
    ));
    for row in image.rows() {
        let pels = row.map(|pixel| {
            if pixel.0[0] < 128 {
                Color::Black
            } else {
                Color::White
            }
        });
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

use image::RgbImage;
use imageproc::region_labelling::{Connectivity, connected_components};

const SAUVOLA_WINDOW: u32 = 41; // px at 300 dpi
const SAUVOLA_K: f64 = 0.30;
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
    let raw = gray.as_raw();
    for y in 0..h {
        let mut row_sum = 0_u64;
        let mut row_sq = 0_u64;
        for x in 0..w {
            let v = raw[y * w + x] as u64;
            row_sum += v;
            row_sq += v * v;
            sum[(y + 1) * stride + x + 1] = sum[y * stride + x + 1] + row_sum;
            sq[(y + 1) * stride + x + 1] = sq[y * stride + x + 1] + row_sq;
        }
    }
    let r = (SAUVOLA_WINDOW / 2) as isize;
    let mut out = GrayImage::from_pixel(gray.width(), gray.height(), Luma([255]));
    let out_raw = out.as_mut();
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
            if (raw[y * w + x] as f64) < threshold {
                out_raw[y * w + x] = 0;
            }
        }
    }
    out
}

/// Drops (1) black components touching the image border that are not thin
/// rules, and (2) components of ≤ 3 pixels.
fn remove_border_and_specks(bilevel: &mut GrayImage) {
    let (w, h) = bilevel.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    let labels = connected_components(bilevel, Connectivity::Four, Luma([255]));
    let count = labels.pixels().map(|p| p.0[0]).max().unwrap_or(0) as usize + 1;
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
    if !drop.iter().any(|d| *d) {
        return;
    }
    for (x, y, label) in labels.enumerate_pixels() {
        let l = label.0[0] as usize;
        if l != 0 && drop[l] {
            bilevel.put_pixel(x, y, Luma([255]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(width: u32, height: u32) -> GrayImage {
        GrayImage::from_fn(width, height, |x, y| {
            if (x / 3 + y / 5) % 2 == 0 {
                Luma([0])
            } else {
                Luma([255])
            }
        })
    }

    #[test]
    fn g4_round_trip_is_pixel_identical() {
        let mut image = checker(37, 11); // odd width, mixed rows
        for x in 0..37 {
            image.put_pixel(x, 0, Luma([255])); // all-white row
        }
        for x in 0..37 {
            image.put_pixel(x, 1, Luma([0])); // all-black row
        }
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
        assert_eq!(
            decode_g4(&bytes, 2480, 3508).expect("decode").as_raw(),
            image.as_raw()
        );
    }

    #[test]
    fn truncated_g4_is_an_error_not_a_half_page() {
        let bytes = encode_g4(&checker(64, 64));
        let cut = &bytes[..bytes.len() / 3];
        assert!(decode_g4(cut, 64, 64).is_err());
    }

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
                for dy in 0..12 {
                    for dx in 0..18 {
                        page.put_pixel(x + dx, y0 + dy, Rgb([20, 20, 20]));
                    }
                }
            }
        }
        // Specks (2x1 px) scattered on the paper.
        for y in (30..h).step_by(97) {
            page.put_pixel(25, y, Rgb([0, 0, 0]));
            page.put_pixel(26, y, Rgb([0, 0, 0]));
        }
        // Dark rim on the right edge, 6 px wide, full height.
        for y in 0..h {
            for x in w - 6..w {
                page.put_pixel(x, y, Rgb([30, 30, 30]));
            }
        }
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
        for row in 0..8 {
            let y0 = 60 + row * 80;
            for x in (40..500).step_by(30) {
                for dy in 0..12 {
                    for dx in 0..18 {
                        if out.get_pixel(x + dx, y0 + dy).0[0] == 0 {
                            kept += 1;
                        }
                    }
                }
            }
        }
        assert!(kept >= 27_648 * 97 / 100, "strokes lost: kept {kept}");
        // The rim is gone (it touched the border and is not a thin rule).
        for y in (0..800).step_by(50) {
            for x in 594..600 {
                assert_eq!(out.get_pixel(x, y).0[0], 255, "rim survived at ({x},{y})");
            }
        }
        // Specks are gone.
        for y in (30..800).step_by(97) {
            assert_eq!(out.get_pixel(25, y).0[0], 255, "speck at y={y}");
        }
        // Total ink ≈ strokes only (no more than +3 %).
        assert!(
            black_count(&out) <= 27_648 * 103 / 100,
            "extra ink: {}",
            black_count(&out)
        );
    }

    #[test]
    fn binarize_keeps_a_full_width_rule_touching_the_border() {
        // A 1-px rule across the full width (0.33 % of the height) touches the
        // border on both ends — it is content, not the mat rim.
        let mut page = RgbImage::from_pixel(400, 300, Rgb([240, 240, 240]));
        for x in 0..400 {
            page.put_pixel(x, 150, Rgb([10, 10, 10]));
        }
        let out = binarize(&page);
        assert_eq!(out.get_pixel(200, 150).0[0], 0, "table rule was erased");
        assert_eq!(out.get_pixel(0, 150).0[0], 0);
    }

    #[test]
    fn binarize_blank_page_is_white() {
        let page = RgbImage::from_pixel(300, 200, Rgb([235, 238, 240]));
        assert_eq!(black_count(&binarize(&page)), 0);
    }
}
