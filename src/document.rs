use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageReader, Rgb, RgbImage, imageops};
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use printpdf::{
    ImageCompression, ImageOptimizationOptions, Mm, Op, PdfDocument, PdfPage, PdfSaveOptions, Pt,
    RawImage, XObjectTransform,
};
use std::fs;
use std::io::Cursor;
use std::path::Path;

pub const A4_WIDTH_PX: u32 = 2480;
pub const A4_HEIGHT_PX: u32 = 3508;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropPoint {
    pub x: f32,
    pub y: f32,
}

impl CropPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

#[derive(Clone)]
pub struct ScannedPage {
    pub jpeg: Vec<u8>,
    pub review_image: RgbImage,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetectResult {
    pub corners: [CropPoint; 4],
    pub confident: bool,
}

pub fn detect_document_corners(image: &RgbImage) -> [CropPoint; 4] {
    detect_document(image).corners
}

/// Contour-first detection: the page is the largest bright region on the dark
/// scanner mat, so its outer boundary is found by thresholding — interior
/// content (table rules, dense text) cannot hijack the crop the way strong
/// Hough lines could.
pub fn detect_document(image: &RgbImage) -> DetectResult {
    let fallback = DetectResult {
        corners: fallback_corners(),
        confident: false,
    };
    if image.width() < 40 || image.height() < 40 {
        return fallback;
    }
    let preview = resize_to_fit(image, 720, 720, imageops::FilterType::Triangle);
    let gray = DynamicImage::ImageRgb8(preview).to_luma8();
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    if width < 40 || height < 40 {
        return fallback;
    }

    // Threshold midway between the dark and bright population peaks.
    let mut histogram = [0_u64; 256];
    for pixel in gray.pixels() {
        histogram[pixel[0] as usize] += 1;
    }
    let total: u64 = histogram.iter().sum();
    let dark = percentile(&histogram, total / 10) as u32;
    let bright = percentile(&histogram, total * 9 / 10) as u32;
    if bright < dark + 50 {
        return fallback;
    }
    let threshold = ((dark + bright) / 2) as u8;

    // Largest connected bright component via two-pass union-find labelling.
    let mask: Vec<bool> = gray.pixels().map(|pixel| pixel[0] > threshold).collect();
    let mut labels = vec![0_u32; width * height];
    let mut parents: Vec<u32> = vec![0];
    fn find(parents: &mut Vec<u32>, mut label: u32) -> u32 {
        while parents[label as usize] != label {
            parents[label as usize] = parents[parents[label as usize] as usize];
            label = parents[label as usize];
        }
        label
    }
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            if !mask[index] {
                continue;
            }
            let left = if x > 0 { labels[index - 1] } else { 0 };
            let up = if y > 0 { labels[index - width] } else { 0 };
            labels[index] = match (left, up) {
                (0, 0) => {
                    let label = parents.len() as u32;
                    parents.push(label);
                    label
                }
                (label, 0) | (0, label) => label,
                (first, second) => {
                    let first_root = find(&mut parents, first);
                    let second_root = find(&mut parents, second);
                    if first_root != second_root {
                        let min_root = first_root.min(second_root);
                        parents[first_root.max(second_root) as usize] = min_root;
                        min_root
                    } else {
                        first_root
                    }
                }
            };
        }
    }
    let mut areas = vec![0_u64; parents.len()];
    for &label in &labels {
        if label != 0 {
            let root = find(&mut parents, label);
            areas[root as usize] += 1;
        }
    }
    let Some((component, &area)) = areas
        .iter()
        .enumerate()
        .skip(1)
        .max_by_key(|(_, area)| **area)
    else {
        return fallback;
    };
    if area < (width * height) as u64 * 18 / 100 {
        return fallback;
    }
    let component = component as u32;

    // Corner estimate: extreme points of the component (rotation-tolerant).
    let mut top_left = (0_f32, 0_f32);
    let mut top_right = (0_f32, 0_f32);
    let mut bottom_right = (0_f32, 0_f32);
    let mut bottom_left = (0_f32, 0_f32);
    let (mut min_sum, mut max_sum) = (f32::INFINITY, f32::NEG_INFINITY);
    let (mut min_diff, mut max_diff) = (f32::INFINITY, f32::NEG_INFINITY);
    let mut inside_brightness = 0_u64;
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            if labels[index] == 0 || find(&mut parents, labels[index]) != component {
                continue;
            }
            inside_brightness += gray.get_pixel(x as u32, y as u32)[0] as u64;
            let (fx, fy) = (x as f32, y as f32);
            if fx + fy < min_sum {
                min_sum = fx + fy;
                top_left = (fx, fy);
            }
            if fx + fy > max_sum {
                max_sum = fx + fy;
                bottom_right = (fx, fy);
            }
            if fx - fy > max_diff {
                max_diff = fx - fy;
                top_right = (fx, fy);
            }
            if fx - fy < min_diff {
                min_diff = fx - fy;
                bottom_left = (fx, fy);
            }
        }
    }
    let corners = [top_left, top_right, bottom_right, bottom_left];
    if !valid_quadrilateral(corners, width as f32, height as f32) {
        return fallback;
    }

    // Confidence: page interior must clearly outshine the surrounding mat.
    let outside_area = (width * height) as u64 - area;
    let total_brightness: u64 = gray.pixels().map(|pixel| pixel[0] as u64).sum();
    let inside_mean = inside_brightness / area.max(1);
    let outside_mean = total_brightness.saturating_sub(inside_brightness) / outside_area.max(1);
    let confident = inside_mean >= outside_mean + 40 && outside_area > 0;

    DetectResult {
        corners: corners
            .map(|(x, y)| CropPoint::new(x / width as f32, y / height as f32)),
        confident,
    }
}

pub fn process_page(image: &RgbImage, corners: [CropPoint; 4]) -> Result<ScannedPage, String> {
    let corners = expand_corners(corners, 0.018);
    let source = corners.map(|point| {
        (
            point.x.clamp(0.0, 1.0) * (image.width() - 1) as f32,
            point.y.clamp(0.0, 1.0) * (image.height() - 1) as f32,
        )
    });
    let target = [
        (0.0, 0.0),
        ((A4_WIDTH_PX - 1) as f32, 0.0),
        ((A4_WIDTH_PX - 1) as f32, (A4_HEIGHT_PX - 1) as f32),
        (0.0, (A4_HEIGHT_PX - 1) as f32),
    ];
    let projection = Projection::from_control_points(source, target)
        .ok_or_else(|| "Nieprawidłowe ustawienie narożników kadrowania.".to_owned())?;
    let mut output = RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([255, 255, 255]));
    warp_into(
        image,
        projection,
        Interpolation::Bilinear,
        Border::Constant(Rgb([255, 255, 255])),
        &mut output,
    );
    enhance_document(&mut output);
    page_from_image(output)
}

fn valid_quadrilateral(corners: [(f32, f32); 4], width: f32, height: f32) -> bool {
    let distance = |left: (f32, f32), right: (f32, f32)| (right.0 - left.0).hypot(right.1 - left.1);
    let top_width = distance(corners[0], corners[1]);
    let bottom_width = distance(corners[3], corners[2]);
    let left_height = distance(corners[0], corners[3]);
    let right_height = distance(corners[1], corners[2]);
    let area = corners
        .iter()
        .zip(corners.iter().cycle().skip(1))
        .take(4)
        .map(|(left, right)| left.0 * right.1 - right.0 * left.1)
        .sum::<f32>()
        .abs()
        * 0.5;
    top_width > width * 0.25
        && bottom_width > width * 0.25
        && left_height > height * 0.25
        && right_height > height * 0.25
        && area > width * height * 0.18
        && corners.iter().all(|(x, y)| {
            *x >= -width * 0.05 && *x <= width * 1.05 && *y >= -height * 0.05 && *y <= height * 1.05
        })
}

fn expand_corners(corners: [CropPoint; 4], amount: f32) -> [CropPoint; 4] {
    let center = CropPoint::new(
        corners.iter().map(|point| point.x).sum::<f32>() / 4.0,
        corners.iter().map(|point| point.y).sum::<f32>() / 4.0,
    );
    corners.map(|point| {
        CropPoint::new(
            (center.x + (point.x - center.x) * (1.0 + amount)).clamp(0.0, 1.0),
            (center.y + (point.y - center.y) * (1.0 + amount)).clamp(0.0, 1.0),
        )
    })
}

pub fn rotate_page_clockwise(page: &ScannedPage) -> Result<ScannedPage, String> {
    let image = decode_jpeg(&page.jpeg)?;
    page_from_image(imageops::rotate90(&image))
}

pub fn save_pdf(path: &Path, pages: &[&ScannedPage]) -> Result<(), String> {
    if pages.is_empty() {
        return Err("Dokument nie zawiera żadnych stron.".to_owned());
    }
    let mut document = PdfDocument::new("Zeskanowany dokument");
    let mut pdf_pages = Vec::with_capacity(pages.len());
    for page in pages {
        let mut warnings = Vec::new();
        let image = RawImage::decode_from_bytes(&page.jpeg, &mut warnings)
            .map_err(|error| format!("Nie można przygotować strony PDF: {error}"))?;
        let image_id = document.add_image(&image);
        let landscape = page.width > page.height;
        let (page_width, page_height) = if landscape {
            (Mm(297.0), Mm(210.0))
        } else {
            (Mm(210.0), Mm(297.0))
        };
        let operations = vec![Op::UseXobject {
            id: image_id,
            transform: XObjectTransform {
                translate_x: Some(Pt(0.0)),
                translate_y: Some(Pt(0.0)),
                dpi: Some(300.0),
                ..Default::default()
            },
        }];
        pdf_pages.push(PdfPage::new(page_width, page_height, operations));
    }
    let save_options = PdfSaveOptions {
        image_optimization: Some(ImageOptimizationOptions {
            quality: Some(0.93),
            max_image_size: None,
            dither_greyscale: None,
            convert_to_greyscale: None,
            auto_optimize: None,
            format: Some(ImageCompression::Jpeg),
        }),
        ..PdfSaveOptions::default()
    };
    let bytes = document
        .with_pages(pdf_pages)
        .save(&save_options, &mut Vec::new());
    let part_path = path.with_extension("pdf.part");
    fs::write(&part_path, bytes)
        .map_err(|error| format!("Nie można zapisać pliku PDF: {error}"))?;
    if let Err(error) = fs::rename(&part_path, path) {
        let _ = fs::remove_file(&part_path);
        return Err(format!("Nie można ukończyć zapisu pliku PDF: {error}"));
    }
    Ok(())
}

fn page_from_image(image: RgbImage) -> Result<ScannedPage, String> {
    let width = image.width();
    let height = image.height();
    let review_image = resize_to_fit(&image, 1200, 1200, imageops::FilterType::Lanczos3);
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 91)
        .encode_image(&image)
        .map_err(|error| format!("Nie można skompresować zeskanowanej strony: {error}"))?;
    Ok(ScannedPage {
        jpeg,
        review_image,
        width,
        height,
    })
}

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

fn decode_jpeg(bytes: &[u8]) -> Result<RgbImage, String> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?
        .decode()
        .map_err(|error| format!("Nie można odczytać zeskanowanej strony: {error}"))
        .map(|image| image.to_rgb8())
}

fn resize_to_fit(
    image: &RgbImage,
    max_width: u32,
    max_height: u32,
    filter: imageops::FilterType,
) -> RgbImage {
    let scale = (max_width as f64 / image.width() as f64)
        .min(max_height as f64 / image.height() as f64)
        .min(1.0);
    let width = (image.width() as f64 * scale).round().max(1.0) as u32;
    let height = (image.height() as f64 * scale).round().max(1.0) as u32;
    imageops::resize(image, width, height, filter)
}

fn enhance_document(image: &mut RgbImage) {
    let width = image.width();
    let height = image.height();
    if width < 20 || height < 20 {
        return;
    }
    // Sample only the central region: page borders (dark mat) and lamp
    // reflections near the edges must not skew the paper-color estimate.
    let x_range = width / 5..width - width / 5;
    let y_range = height / 5..height - height / 5;
    let mut histograms = [[0_u64; 256]; 3];
    let mut sample_count = 0_u64;
    for y in y_range.step_by(4) {
        for x in x_range.clone().step_by(4) {
            let pixel = image.get_pixel(x, y);
            for channel in 0..3 {
                histograms[channel][pixel[channel] as usize] += 1;
            }
            sample_count += 1;
        }
    }
    if sample_count == 0 {
        return;
    }
    let mut lows = [0_usize; 3];
    let mut papers = [0_usize; 3];
    for channel in 0..3 {
        lows[channel] = percentile(&histograms[channel], sample_count / 200);
        // Paper white = the dominant bright value, immune to small specular
        // highlights that fool a high percentile.
        papers[channel] = bright_mode(&histograms[channel]);
    }
    if (0..3).any(|channel| papers[channel] <= lows[channel] + 60) {
        return;
    }
    let scales: [f32; 3] =
        std::array::from_fn(|channel| 242.0 / (papers[channel] - lows[channel]) as f32);
    for pixel in image.pixels_mut() {
        for channel in 0..3 {
            let adjusted =
                (pixel[channel] as i32 - lows[channel] as i32) as f32 * scales[channel] + 5.0;
            pixel[channel] = adjusted.round().clamp(0.0, 255.0) as u8;
        }
    }
}

fn bright_mode(histogram: &[u64; 256]) -> usize {
    histogram
        .iter()
        .enumerate()
        .skip(100)
        .max_by_key(|(_, count)| **count)
        .map(|(value, _)| value)
        .unwrap_or(255)
}

fn percentile(histogram: &[u64; 256], target: u64) -> usize {
    let mut count = 0;
    for (value, occurrences) in histogram.iter().enumerate() {
        count += occurrences;
        if count >= target {
            return value;
        }
    }
    255
}

fn fallback_corners() -> [CropPoint; 4] {
    [
        CropPoint::new(0.06, 0.06),
        CropPoint::new(0.94, 0.06),
        CropPoint::new(0.94, 0.94),
        CropPoint::new(0.06, 0.94),
    ]
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_for_tiny_image() {
        let image = RgbImage::new(10, 10);
        assert_eq!(detect_document_corners(&image), fallback_corners());
    }

    #[test]
    fn detects_a_perspective_document() {
        let mut image = RgbImage::from_pixel(800, 600, Rgb([25, 25, 25]));
        let polygon = [
            imageproc::point::Point::new(210, 75),
            imageproc::point::Point::new(610, 105),
            imageproc::point::Point::new(655, 525),
            imageproc::point::Point::new(165, 500),
        ];
        imageproc::drawing::draw_polygon_mut(&mut image, &polygon, Rgb([245, 245, 245]));
        let detected = detect_document_corners(&image);
        let expected = [
            CropPoint::new(210.0 / 800.0, 75.0 / 600.0),
            CropPoint::new(610.0 / 800.0, 105.0 / 600.0),
            CropPoint::new(655.0 / 800.0, 525.0 / 600.0),
            CropPoint::new(165.0 / 800.0, 500.0 / 600.0),
        ];
        for (actual, expected) in detected.into_iter().zip(expected) {
            assert!((actual.x - expected.x).abs() < 0.06);
            assert!((actual.y - expected.y).abs() < 0.06);
        }
    }

    #[test]
    fn review_image_keeps_a4_proportions() {
        let image = RgbImage::from_pixel(210, 297, Rgb([255, 255, 255]));
        let review = resize_to_fit(&image, 120, 120, imageops::FilterType::Lanczos3);
        assert_eq!(review.dimensions(), (85, 120));
    }

    #[test]
    #[ignore = "narzędzie diagnostyczne dla prawdziwej klatki IRIScan"]
    fn processes_an_external_camera_frame() {
        let input = std::env::var("IRISCAN_TEST_FRAME").expect("IRISCAN_TEST_FRAME");
        let output = std::env::var("IRISCAN_TEST_CROP").expect("IRISCAN_TEST_CROP");
        let image = image::open(input).expect("odczyt klatki").to_rgb8();
        let corners = detect_document_corners(&image);
        println!("wykryte narożniki: {corners:?}");
        let page = process_page(&image, corners).expect("przetworzenie strony");
        page.review_image.save(output).expect("zapis podglądu");
    }

    #[test]
    fn table_lines_do_not_hijack_the_crop() {
        let mut image = RgbImage::from_pixel(800, 600, Rgb([25, 25, 25]));
        for y in 60..540 {
            for x in 100..700 {
                image.put_pixel(x, y, Rgb([240, 240, 240]));
            }
        }
        for line in 0..4 {
            let y = 180 + line * 80;
            for x in 160..640 {
                for dy in 0..3 {
                    image.put_pixel(x, y + dy, Rgb([30, 30, 30]));
                }
            }
        }
        for line in 0..5 {
            let x = 200 + line * 100;
            for y in 180..460 {
                for dx in 0..3 {
                    image.put_pixel(x + dx, y, Rgb([30, 30, 30]));
                }
            }
        }
        let result = detect_document(&image);
        assert!(result.confident, "strona na macie powinna być pewna");
        let expected = [
            CropPoint::new(100.0 / 800.0, 60.0 / 600.0),
            CropPoint::new(699.0 / 800.0, 60.0 / 600.0),
            CropPoint::new(699.0 / 800.0, 539.0 / 600.0),
            CropPoint::new(100.0 / 800.0, 539.0 / 600.0),
        ];
        for (actual, expected) in result.corners.into_iter().zip(expected) {
            assert!(
                (actual.x - expected.x).abs() < 0.03,
                "narożnik x wpadł w tabelę: {actual:?}"
            );
            assert!(
                (actual.y - expected.y).abs() < 0.03,
                "narożnik y wpadł w tabelę: {actual:?}"
            );
        }
    }

    #[test]
    fn empty_mat_is_not_confident() {
        let image = RgbImage::from_pixel(640, 480, Rgb([30, 30, 35]));
        let result = detect_document(&image);
        assert!(!result.confident, "pusta mata nie może być pewna");
    }

    #[test]
    fn ignores_borders_and_highlights_when_estimating_paper() {
        let mut image = RgbImage::from_pixel(400, 400, Rgb([20, 20, 25]));
        for y in 40..360 {
            for x in 40..360 {
                image.put_pixel(x, y, Rgb([185, 195, 225]));
            }
        }
        for y in 180..240 {
            for x in 180..240 {
                image.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        for y in 120..150 {
            for x in 120..280 {
                image.put_pixel(x, y, Rgb([40, 45, 70]));
            }
        }
        enhance_document(&mut image);
        let paper = image.get_pixel(100, 100);
        assert!(
            paper[0] >= 240 && paper[1] >= 240 && paper[2] >= 240,
            "papier nie został rozjaśniony: {:?}",
            paper
        );
        let gap = paper[0]
            .abs_diff(paper[1])
            .max(paper[1].abs_diff(paper[2]))
            .max(paper[0].abs_diff(paper[2]));
        assert!(gap <= 6, "papier nie jest neutralny: {:?}", paper);
    }

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
        assert!(max_channel_gap <= 6, "tło nie jest neutralne: {:?}", corner);
    }

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

    #[test]
    fn writes_a_valid_pdf() {
        let image = RgbImage::from_pixel(100, 141, Rgb([245, 245, 245]));
        let page = page_from_image(image).expect("strona testowa");
        let path =
            std::env::temp_dir().join(format!("skaner-dokumentow-test-{}.pdf", std::process::id()));
        save_pdf(&path, &[&page]).expect("zapis PDF");
        let bytes = std::fs::read(&path).expect("odczyt PDF");
        assert!(bytes.starts_with(b"%PDF-"));
        std::fs::remove_file(path).expect("usunięcie testowego PDF");
    }
}
