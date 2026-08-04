use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageReader, Rgb, RgbImage, imageops};
use imageproc::edges::canny;
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use imageproc::hough::{LineDetectionOptions, PolarLine, detect_lines};
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

pub fn detect_document_corners(image: &RgbImage) -> [CropPoint; 4] {
    if image.width() < 40 || image.height() < 40 {
        return fallback_corners();
    }
    let preview = resize_to_fit(image, 720, 720, imageops::FilterType::Triangle);
    let gray = DynamicImage::ImageRgb8(preview).to_luma8();
    let width = gray.width() as usize;
    let height = gray.height() as usize;
    if width < 40 || height < 40 {
        return fallback_corners();
    }

    let mut vertical_scores = vec![0.0_f64; width];
    let mut horizontal_scores = vec![0.0_f64; height];
    let y_margin = (height / 20).max(2);
    let x_margin = (width / 20).max(2);

    for y in y_margin..height.saturating_sub(y_margin) {
        for (x, score) in vertical_scores
            .iter_mut()
            .enumerate()
            .take(width - 1)
            .skip(1)
        {
            let left = gray.get_pixel((x - 1) as u32, y as u32)[0] as f64;
            let right = gray.get_pixel((x + 1) as u32, y as u32)[0] as f64;
            *score += (right - left).abs();
        }
    }
    for (y, score) in horizontal_scores
        .iter_mut()
        .enumerate()
        .take(height - 1)
        .skip(1)
    {
        for x in x_margin..width.saturating_sub(x_margin) {
            let top = gray.get_pixel(x as u32, (y - 1) as u32)[0] as f64;
            let bottom = gray.get_pixel(x as u32, (y + 1) as u32)[0] as f64;
            *score += (bottom - top).abs();
        }
    }

    let vertical_scores = smooth_scores(&vertical_scores, 7);
    let horizontal_scores = smooth_scores(&horizontal_scores, 7);
    let left = strongest_index(&vertical_scores, width / 40, width * 9 / 20);
    let right = strongest_index(&vertical_scores, width * 11 / 20, width * 39 / 40);
    let top = strongest_index(&horizontal_scores, height / 40, height * 9 / 20);
    let bottom = strongest_index(&horizontal_scores, height * 11 / 20, height * 39 / 40);

    let (Some(left), Some(right), Some(top), Some(bottom)) = (left, right, top, bottom) else {
        return fallback_corners();
    };
    if right.saturating_sub(left) < width / 3 || bottom.saturating_sub(top) < height / 3 {
        return fallback_corners();
    }
    let rectangle = [
        (left as f32, top as f32),
        (right as f32, top as f32),
        (right as f32, bottom as f32),
        (left as f32, bottom as f32),
    ];
    let corners = perspective_corners(&gray, rectangle).unwrap_or(rectangle);
    corners.map(|(x, y)| CropPoint::new(x / width as f32, y / height as f32))
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

fn perspective_corners(
    gray: &image::GrayImage,
    rectangle: [(f32, f32); 4],
) -> Option<[(f32, f32); 4]> {
    let edges = canny(gray, 35.0, 90.0);
    let threshold = (gray.width().min(gray.height()) / 5).max(55);
    let lines = detect_lines(
        &edges,
        LineDetectionOptions {
            vote_threshold: threshold,
            suppression_radius: 8,
        },
    );
    let center_x = gray.width() as f32 * 0.5;
    let center_y = gray.height() as f32 * 0.5;
    let left = closest_vertical_line(&lines, rectangle[0].0, center_y, gray.width())?;
    let right = closest_vertical_line(&lines, rectangle[1].0, center_y, gray.width())?;
    let top = closest_horizontal_line(&lines, rectangle[0].1, center_x, gray.height())?;
    let bottom = closest_horizontal_line(&lines, rectangle[2].1, center_x, gray.height())?;
    let corners = [
        line_intersection(left, top)?,
        line_intersection(right, top)?,
        line_intersection(right, bottom)?,
        line_intersection(left, bottom)?,
    ];
    valid_quadrilateral(corners, gray.width() as f32, gray.height() as f32).then_some(corners.map(
        |(x, y)| {
            (
                x.clamp(0.0, gray.width() as f32 - 1.0),
                y.clamp(0.0, gray.height() as f32 - 1.0),
            )
        },
    ))
}

fn closest_vertical_line(
    lines: &[PolarLine],
    expected_x: f32,
    center_y: f32,
    width: u32,
) -> Option<PolarLine> {
    lines
        .iter()
        .copied()
        .filter(|line| line.angle_in_degrees <= 18 || line.angle_in_degrees >= 162)
        .filter_map(|line| {
            let angle = (line.angle_in_degrees as f32).to_radians();
            let (sin, cos) = angle.sin_cos();
            (cos.abs() > 0.1).then(|| {
                let x = (line.r - center_y * sin) / cos;
                (line, x)
            })
        })
        .filter(|(_, x)| *x >= 0.0 && *x < width as f32)
        .min_by(|(_, left_x), (_, right_x)| {
            (left_x - expected_x)
                .abs()
                .total_cmp(&(right_x - expected_x).abs())
        })
        .map(|(line, _)| line)
}

fn closest_horizontal_line(
    lines: &[PolarLine],
    expected_y: f32,
    center_x: f32,
    height: u32,
) -> Option<PolarLine> {
    lines
        .iter()
        .copied()
        .filter(|line| line.angle_in_degrees.abs_diff(90) <= 18)
        .filter_map(|line| {
            let angle = (line.angle_in_degrees as f32).to_radians();
            let (sin, cos) = angle.sin_cos();
            (sin.abs() > 0.1).then(|| {
                let y = (line.r - center_x * cos) / sin;
                (line, y)
            })
        })
        .filter(|(_, y)| *y >= 0.0 && *y < height as f32)
        .min_by(|(_, left_y), (_, right_y)| {
            (left_y - expected_y)
                .abs()
                .total_cmp(&(right_y - expected_y).abs())
        })
        .map(|(line, _)| line)
}

fn line_intersection(first: PolarLine, second: PolarLine) -> Option<(f32, f32)> {
    let first_angle = (first.angle_in_degrees as f32).to_radians();
    let second_angle = (second.angle_in_degrees as f32).to_radians();
    let (first_sin, first_cos) = first_angle.sin_cos();
    let (second_sin, second_cos) = second_angle.sin_cos();
    let determinant = first_cos * second_sin - second_cos * first_sin;
    if determinant.abs() < 0.01 {
        return None;
    }
    Some((
        (first.r * second_sin - second.r * first_sin) / determinant,
        (first_cos * second.r - second_cos * first.r) / determinant,
    ))
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

fn smooth_scores(scores: &[f64], radius: usize) -> Vec<f64> {
    let mut smoothed = vec![0.0; scores.len()];
    for (index, output) in smoothed.iter_mut().enumerate() {
        let start = index.saturating_sub(radius);
        let end = (index + radius + 1).min(scores.len());
        *output = scores[start..end].iter().sum::<f64>() / (end - start) as f64;
    }
    smoothed
}

fn strongest_index(scores: &[f64], start: usize, end: usize) -> Option<usize> {
    if start >= end || end > scores.len() {
        return None;
    }
    scores[start..end]
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(offset, _)| start + offset)
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
