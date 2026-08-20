use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageReader, Rgb, RgbImage, imageops};
use imageproc::geometric_transformations::{Border, Interpolation, Projection, warp_into};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::Path;

pub const A4_WIDTH_PX: u32 = 2480;
pub const A4_HEIGHT_PX: u32 = 3508;
/// Film-strip thumbnails draw at ≤96 pt (~170 px at 175% DPI); 320 px keeps
/// them crisp at a fraction of the former 1200 px footprint (~0.2 MB RAM and
/// ~0.3 MB VRAM per page instead of ~3 MB + 4 MB).
const STRIP_THUMB_PX: u32 = 320;
/// JPEG quality for `ColorMode::Color` pages (colour is opt-in for the rare
/// stamped/coloured document; default pages are bilevel G4).
const COLOR_JPEG_QUALITY: u8 = 80;
const FLATTEN_COLS: usize = 12;
const FLATTEN_ROWS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CropPoint {
    pub x: f32,
    pub y: f32,
}

impl CropPoint {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

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
    // Bias the cut toward dark so shadowed paper edges stay inside the page
    // component (the mat is near-black, so plenty of margin remains).
    let threshold = (dark + (bright - dark) * 35 / 100) as u8;

    // Largest connected bright component via two-pass union-find labelling.
    let mask: Vec<bool> = gray.pixels().map(|pixel| pixel[0] > threshold).collect();
    let mut labels = vec![0_u32; width * height];
    let mut parents: Vec<u32> = vec![0];
    fn find(parents: &mut [u32], mut label: u32) -> u32 {
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
        corners: corners.map(|(x, y)| CropPoint::new(x / width as f32, y / height as f32)),
        confident,
    }
}

/// Test-only shorthand: detected corners, default (black-and-white) mode.
#[cfg(test)]
#[cfg(test)]
pub fn process_page(image: &RgbImage, corners: [CropPoint; 4]) -> Result<ScannedPage, String> {
    process_page_with(image, corners, true, ColorMode::default())
}

/// `expand_detected` grows the quad slightly to swallow the detection shadow.
/// Hand-placed editor corners must be applied exactly as dragged, so reprocess
/// jobs pass `false`.
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

/// Geometry only: perspective-corrects the page onto an A4 canvas without any
/// tone or colour work (used by the version-comparison harness).
///
/// The canvas orientation follows the detected quad: a page lying sideways is
/// warped onto a landscape A4 canvas instead of being stretched onto portrait.
fn warp_only(
    image: &RgbImage,
    corners: [CropPoint; 4],
    expand_detected: bool,
) -> Result<RgbImage, String> {
    let corners = if expand_detected {
        expand_corners(corners, 0.018)
    } else {
        corners
    };
    let source = corners.map(|point| {
        (
            point.x.clamp(0.0, 1.0) * (image.width() - 1) as f32,
            point.y.clamp(0.0, 1.0) * (image.height() - 1) as f32,
        )
    });
    let distance = |left: (f32, f32), right: (f32, f32)| (right.0 - left.0).hypot(right.1 - left.1);
    let source_width = (distance(source[0], source[1]) + distance(source[3], source[2])) * 0.5;
    let source_height = (distance(source[0], source[3]) + distance(source[1], source[2])) * 0.5;
    let (target_width, target_height) = if source_width > source_height {
        (A4_HEIGHT_PX, A4_WIDTH_PX)
    } else {
        (A4_WIDTH_PX, A4_HEIGHT_PX)
    };
    let target = [
        (0.0, 0.0),
        ((target_width - 1) as f32, 0.0),
        ((target_width - 1) as f32, (target_height - 1) as f32),
        (0.0, (target_height - 1) as f32),
    ];
    let projection = Projection::from_control_points(source, target)
        .ok_or_else(|| "Nieprawidłowe ustawienie narożników kadrowania.".to_owned())?;
    let mut output = RgbImage::from_pixel(target_width, target_height, Rgb([255, 255, 255]));
    warp_into(
        image,
        projection,
        Interpolation::Bilinear,
        Border::Constant(Rgb([255, 255, 255])),
        &mut output,
    );
    Ok(output)
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

/// Display-time rotation. Page JPEGs are never re-encoded for rotation any
/// more — `quarter_turns` travels as metadata (session + PDF `/Rotate`) and
/// only rasters headed for the screen are rotated.
pub fn rotate_rgb(image: &RgbImage, quarter_turns: u8) -> RgbImage {
    match quarter_turns % 4 {
        1 => imageops::rotate90(image),
        2 => imageops::rotate180(image),
        3 => imageops::rotate270(image),
        _ => image.clone(),
    }
}

const PDF_TITLE: &str = "Zeskanowany dokument";
const PDF_EDITABLE_MARKER: &str = "skaner-dokumentow-editable-v1";
const A4_PORTRAIT_PT: (f32, f32) = (210.0 / 25.4 * 72.0, 297.0 / 25.4 * 72.0);

/// Writes each page's encoded bytes directly as an image stream: JPEG pages as
/// `DCTDecode/DeviceRGB`, bilevel pages as `CCITTFaxDecode/DeviceGray` 1 bpc.
///
/// No decode or re-encode happens here: the PDF carries the exact bytes held
/// in RAM, so saving is fast, needs no per-page raster buffers, and repeated
/// edit → save cycles add no generational loss. Rotation is expressed through
/// the page's `/Rotate` entry instead of touching pixels. The layout mirrors
/// exactly what `extract_pdf_pages` accepts back.
pub fn render_pdf(pages: &[(&ScannedPage, u8)]) -> Result<Vec<u8>, String> {
    use lopdf::content::{Content, Operation};
    use lopdf::{Object, Stream, dictionary};

    if pages.is_empty() {
        return Err("Dokument nie zawiera żadnych stron.".to_owned());
    }
    let mut document = lopdf::Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let mut kids = Vec::with_capacity(pages.len());
    for (page, quarter_turns) in pages {
        let landscape = page.width > page.height;
        let (page_width, page_height) = if landscape {
            (A4_PORTRAIT_PT.1, A4_PORTRAIT_PT.0)
        } else {
            A4_PORTRAIT_PT
        };
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
        let image_stream =
            Stream::new(image_dictionary, page.bytes.clone()).with_compression(false);
        let image_id = document.add_object(image_stream);
        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        page_width.into(),
                        0.into(),
                        0.into(),
                        page_height.into(),
                        0.into(),
                        0.into(),
                    ],
                ),
                Operation::new("Do", vec![Object::Name(b"Im0".to_vec())]),
                Operation::new("Q", vec![]),
            ],
        };
        let encoded = content
            .encode()
            .map_err(|error| format!("Nie można przygotować treści strony PDF: {error}"))?;
        let content_id = document.add_object(Stream::new(dictionary! {}, encoded));
        let mut page_dictionary = dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), page_width.into(), page_height.into()],
            "Resources" => dictionary! {
                "XObject" => dictionary! { "Im0" => Object::Reference(image_id) },
            },
            "Contents" => Object::Reference(content_id),
        };
        let rotation = i64::from(quarter_turns % 4) * 90;
        if rotation != 0 {
            page_dictionary.set("Rotate", rotation);
        }
        let page_id = document.add_object(page_dictionary);
        kids.push(Object::Reference(page_id));
    }
    let count = kids.len() as i64;
    document.objects.insert(
        pages_id,
        lopdf::Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    let info_id = document.add_object(dictionary! {
        "Title" => Object::string_literal(PDF_TITLE),
        "Creator" => Object::string_literal("Skaner dokumentów"),
        "Producer" => Object::string_literal("Skaner dokumentów"),
        "Keywords" => Object::string_literal(PDF_EDITABLE_MARKER),
    });
    document.trailer.set("Root", catalog_id);
    document.trailer.set("Info", info_id);
    let mut bytes = Vec::new();
    document
        .save_to(&mut Cursor::new(&mut bytes))
        .map_err(|error| format!("Nie można zapisać PDF: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
pub fn save_pdf(path: &Path, pages: &[(&ScannedPage, u8)]) -> Result<(), String> {
    let bytes = render_pdf(pages)?;
    crate::atomic_file::write(path, bytes)
        .map_err(|error| format!("Nie można ukończyć zapisu pliku PDF: {error}"))
}

fn page_from_image(image: RgbImage, mode: ColorMode) -> Result<ScannedPage, String> {
    let width = image.width();
    let height = image.height();
    match mode {
        ColorMode::Color => {
            let review_image =
                resize_to_fit(&image, STRIP_THUMB_PX, STRIP_THUMB_PX, imageops::FilterType::Lanczos3);
            let mut bytes = Vec::new();
            JpegEncoder::new_with_quality(&mut bytes, COLOR_JPEG_QUALITY)
                .encode_image(&image)
                .map_err(|error| format!("Nie można skompresować zeskanowanej strony: {error}"))?;
            Ok(ScannedPage {
                bytes,
                encoding: PageEncoding::Jpeg,
                review_image,
                width,
                height,
            })
        }
        ColorMode::BlackWhite => {
            let bilevel = crate::bilevel::binarize(&image);
            let bytes = crate::bilevel::encode_g4(&bilevel);
            if bytes.is_empty() {
                return Err("Nie można zakodować strony (G4).".to_owned());
            }
            // The thumbnail shows what is saved, not the colour input.
            let preview = DynamicImage::ImageLuma8(bilevel).to_rgb8();
            let review_image =
                resize_to_fit(&preview, STRIP_THUMB_PX, STRIP_THUMB_PX, imageops::FilterType::Lanczos3);
            Ok(ScannedPage {
                bytes,
                encoding: PageEncoding::G4,
                review_image,
                width,
                height,
            })
        }
    }
}

/// Reads back the pages of a PDF produced by this app: every page is exactly
/// one full-page image XObject (lone `DCTDecode` RGB or lone `CCITTFaxDecode`
/// G4), optionally with a `/Rotate` entry. Returns each page's encoded bytes
/// with its rotation in quarter turns. Foreign PDFs yield an error.
pub fn extract_pdf_pages(path: &Path) -> Result<Vec<(EncodedPage, u8)>, String> {
    let document =
        lopdf::Document::load(path).map_err(|error| format!("Nie można odczytać PDF: {error}"))?;
    let has_marker = has_editable_marker(&document);
    if !has_marker && !pdf_info_string_equals(&document, b"Title", PDF_TITLE) {
        return Err(
            "Ten PDF nie ma bezpiecznego znacznika edycji tej wersji programu. Możesz go otworzyć, ale edycja została zablokowana, aby nie utracić tekstu OCR, adnotacji ani układu strony."
                .to_owned(),
        );
    }
    let page_map = document.get_pages();
    if page_map.is_empty() {
        return Err("Ten PDF nie zawiera stron.".to_owned());
    }
    let catalog = document
        .catalog()
        .map_err(|error| format!("Nie można odczytać katalogu PDF: {error}"))?;
    if has_meaningful_pdf_entry(&document, catalog, b"AcroForm") {
        return Err(
            "Ten PDF zawiera formularz. Edycja mogłaby usunąć jego pola, więc została zablokowana."
                .to_owned(),
        );
    }
    let mut pages = Vec::new();
    for (_number, page_id) in page_map {
        let page_dictionary = document
            .get_object(page_id)
            .and_then(lopdf::Object::as_dict)
            .map_err(|error| format!("Nie można odczytać strony PDF: {error}"))?;
        if has_meaningful_pdf_entry(&document, page_dictionary, b"Annots")
            || has_meaningful_pdf_entry(&document, page_dictionary, b"AA")
        {
            return Err(
                "Ten PDF zawiera adnotacje lub elementy interaktywne. Edycja mogłaby je usunąć, więc została zablokowana."
                    .to_owned(),
            );
        }
        let quarter_turns = page_quarter_turns(&document, page_dictionary).ok_or_else(|| {
            "Ten PDF ma nietypowy obrót strony i nie może być bezpiecznie edytowany.".to_owned()
        })?;
        let (resources_dict, resource_ids) = document
            .get_page_resources(page_id)
            .map_err(|error| format!("Nie można odczytać zasobów strony: {error}"))?;
        let mut dicts: Vec<&lopdf::Dictionary> = Vec::new();
        if let Some(dict) = resources_dict {
            dicts.push(dict);
        }
        for id in resource_ids {
            if let Ok(object) = document.get_object(id)
                && let Ok(dict) = object.as_dict()
            {
                dicts.push(dict);
            }
        }
        // Resources are shared between pages; the page's content stream names
        // which XObject it actually draws (the `Do` operator).
        let mut xobjects: Option<&lopdf::Dictionary> = None;
        for dict in dicts {
            let resolved = match dict.get(b"XObject") {
                Ok(lopdf::Object::Reference(id)) => document
                    .get_object(*id)
                    .ok()
                    .and_then(|object| object.as_dict().ok()),
                Ok(object) => object.as_dict().ok(),
                Err(_) => None,
            };
            if let Some(dict) = resolved {
                xobjects = Some(dict);
                break;
            }
        }
        let Some(xobjects) = xobjects else {
            return Err("Ten PDF nie pochodzi z tego programu.".to_owned());
        };
        let content = document
            .get_and_decode_page_content(page_id)
            .map_err(|error| format!("Nie można odczytać treści strony: {error}"))?;
        let image_name = editable_page_image_name(&content.operations).ok_or_else(|| {
            "Ten PDF zawiera dodatkową treść i nie może być bezpiecznie edytowany.".to_owned()
        })?;
        let value = xobjects
            .get(image_name)
            .map_err(|_| "Ten PDF nie pochodzi z tego programu.".to_owned())?;
        let stream = match value {
            lopdf::Object::Reference(id) => match document.get_object(*id) {
                Ok(lopdf::Object::Stream(stream)) => stream,
                _ => return Err("Ten PDF nie pochodzi z tego programu.".to_owned()),
            },
            lopdf::Object::Stream(stream) => stream,
            _ => return Err("Ten PDF nie pochodzi z tego programu.".to_owned()),
        };
        let is_image = stream
            .dict
            .get(b"Subtype")
            .and_then(|subtype| subtype.as_name())
            .map(|name| name == b"Image")
            .unwrap_or(false);
        let Some(encoding) = stream_page_encoding(&document, stream) else {
            return Err("Ten PDF nie pochodzi z tego programu.".to_owned());
        };
        if !is_image {
            return Err("Ten PDF nie pochodzi z tego programu.".to_owned());
        }
        if !page_is_safe_to_edit(&document, page_dictionary, stream, &content.operations) {
            return Err(
                "Ten PDF nie ma układu pełnej strony rozpoznawanego bezpiecznie przez program. Możesz go otworzyć, ale edycja została zablokowana."
                    .to_owned(),
            );
        }
        let width = stream
            .dict
            .get(b"Width")
            .ok()
            .and_then(pdf_number)
            .map_or(0, |value| value as u32);
        let height = stream
            .dict
            .get(b"Height")
            .ok()
            .and_then(pdf_number)
            .map_or(0, |value| value as u32);
        pages.push((
            EncodedPage {
                bytes: stream.content.clone(),
                encoding,
                width,
                height,
            },
            quarter_turns,
        ));
    }
    Ok(pages)
}

/// `Some(encoding)` when the image stream is exactly one of the two layouts this
/// app writes. A filter chain (e.g. `[FlateDecode, DCTDecode]` would hand back
/// still-compressed bytes), G3, inverted polarity or mismatched Columns/Rows
/// is foreign.
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
    let colorspace: &[u8] = match stream.dict.get(b"ColorSpace") {
        Ok(lopdf::Object::Name(name)) => name,
        _ => return None,
    };
    match filter {
        b"DCTDecode" => {
            (colorspace == b"DeviceRGB" && bpc == Some(8.0)).then_some(PageEncoding::Jpeg)
        }
        b"CCITTFaxDecode" => {
            if colorspace != b"DeviceGray" || bpc != Some(1.0) {
                return None;
            }
            let parms = match stream.dict.get(b"DecodeParms") {
                Ok(lopdf::Object::Dictionary(parms)) => parms,
                Ok(lopdf::Object::Reference(id)) => {
                    document.get_object(*id).ok()?.as_dict().ok()?
                }
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
            if parms.has(b"EncodedByteAlign") || parms.has(b"EndOfBlock") || parms.has(b"EndOfLine")
            {
                return None;
            }
            Some(PageEncoding::G4)
        }
        _ => None,
    }
}

/// `None` for a rotation the app cannot represent (not a multiple of 90°).
fn page_quarter_turns(document: &lopdf::Document, page: &lopdf::Dictionary) -> Option<u8> {
    let value = match page.get(b"Rotate") {
        Ok(lopdf::Object::Reference(id)) => document.get_object(*id).ok()?,
        Ok(value) => value,
        Err(_) => return Some(0),
    };
    let rotation = pdf_number(value)?;
    if rotation.fract() != 0.0 {
        return None;
    }
    let rotation = rotation as i64;
    if rotation % 90 != 0 {
        return None;
    }
    Some((((rotation % 360) + 360) % 360 / 90) as u8)
}

fn editable_page_image_name(operations: &[lopdf::content::Operation]) -> Option<&[u8]> {
    let mut image_name = None;
    for operation in operations {
        match operation.operator.as_str() {
            "q" | "Q" if operation.operands.is_empty() => {}
            "cm" if operation.operands.len() == 6 => {}
            "Do" if operation.operands.len() == 1 && image_name.is_none() => {
                let lopdf::Object::Name(name) = &operation.operands[0] else {
                    return None;
                };
                image_name = Some(name.as_slice());
            }
            _ => return None,
        }
    }
    image_name
}

fn has_meaningful_pdf_entry(
    document: &lopdf::Document,
    dictionary: &lopdf::Dictionary,
    key: &[u8],
) -> bool {
    let Ok(mut value) = dictionary.get(key) else {
        return false;
    };
    if let lopdf::Object::Reference(id) = value {
        let Ok(resolved) = document.get_object(*id) else {
            return true;
        };
        value = resolved;
    }
    match value {
        lopdf::Object::Null => false,
        lopdf::Object::Array(values) => !values.is_empty(),
        lopdf::Object::Dictionary(values) => !values.is_empty(),
        _ => true,
    }
}

fn has_editable_marker(document: &lopdf::Document) -> bool {
    pdf_info_string_contains(document, b"Keywords", PDF_EDITABLE_MARKER)
}

fn pdf_info_string_contains(document: &lopdf::Document, key: &[u8], needle: &str) -> bool {
    pdf_info_string(document, key).is_some_and(|value| pdf_string_contains(value, needle))
}

fn pdf_info_string_equals(document: &lopdf::Document, key: &[u8], expected: &str) -> bool {
    pdf_info_string(document, key).is_some_and(|value| pdf_string_equals(value, expected))
}

fn pdf_info_string<'a>(document: &'a lopdf::Document, key: &[u8]) -> Option<&'a [u8]> {
    let Ok(info) = document.trailer.get(b"Info") else {
        return None;
    };
    let info = match info {
        lopdf::Object::Reference(id) => match document.get_object(*id) {
            Ok(info) => info,
            Err(_) => return None,
        },
        info => info,
    };
    let Ok(dictionary) = info.as_dict() else {
        return None;
    };
    let Ok(lopdf::Object::String(value, _)) = dictionary.get(key) else {
        return None;
    };
    Some(value)
}

fn pdf_string_contains(bytes: &[u8], needle: &str) -> bool {
    if bytes
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
    {
        return true;
    }
    let utf16_be: Vec<u8> = needle.encode_utf16().flat_map(u16::to_be_bytes).collect();
    bytes
        .windows(utf16_be.len())
        .any(|window| window == utf16_be)
}

fn pdf_string_equals(bytes: &[u8], expected: &str) -> bool {
    if bytes == expected.as_bytes() {
        return true;
    }
    let utf16_be: Vec<u8> = expected.encode_utf16().flat_map(u16::to_be_bytes).collect();
    bytes == utf16_be || bytes.strip_prefix(&[0xFE, 0xFF]) == Some(utf16_be.as_slice())
}

fn page_is_safe_to_edit(
    document: &lopdf::Document,
    page: &lopdf::Dictionary,
    image: &lopdf::Stream,
    operations: &[lopdf::content::Operation],
) -> bool {
    // Page-level /Rotate written by this app is handled by the caller;
    // anything the caller could not normalize was already rejected there.
    if page.has(b"UserUnit") || image.dict.has(b"Mask") || image.dict.has(b"SMask") {
        return false;
    }
    if inherited_page_options_present(document, page) {
        return false;
    }
    let dimensions = image
        .dict
        .get(b"Width")
        .ok()
        .and_then(pdf_number)
        .zip(image.dict.get(b"Height").ok().and_then(pdf_number));
    let Some((image_width, image_height)) = dimensions else {
        return false;
    };
    let portrait = approx(image_width, A4_WIDTH_PX as f64, 0.1)
        && approx(image_height, A4_HEIGHT_PX as f64, 0.1);
    let landscape = approx(image_width, A4_HEIGHT_PX as f64, 0.1)
        && approx(image_height, A4_WIDTH_PX as f64, 0.1);
    if !portrait && !landscape {
        return false;
    }

    let Ok(lopdf::Object::Array(media_box)) = page.get(b"MediaBox") else {
        return false;
    };
    if media_box.len() != 4 {
        return false;
    }
    let Some(values) = media_box.iter().map(pdf_number).collect::<Option<Vec<_>>>() else {
        return false;
    };
    let page_width = values[2] - values[0];
    let page_height = values[3] - values[1];
    if !approx(values[0], 0.0, 0.1) || !approx(values[1], 0.0, 0.1) {
        return false;
    }
    for key in [b"CropBox".as_slice(), b"TrimBox", b"BleedBox", b"ArtBox"] {
        let Ok(lopdf::Object::Array(other_box)) = page.get(key) else {
            continue;
        };
        let Some(other_values) = other_box.iter().map(pdf_number).collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        if other_values.len() != 4
            || values
                .iter()
                .zip(other_values.iter())
                .any(|(left, right)| !approx(*left, *right, 0.1))
        {
            return false;
        }
    }
    let expected_page = if portrait {
        (210.0 / 25.4 * 72.0, 297.0 / 25.4 * 72.0)
    } else {
        (297.0 / 25.4 * 72.0, 210.0 / 25.4 * 72.0)
    };
    if !approx(page_width, expected_page.0, 1.0) || !approx(page_height, expected_page.1, 1.0) {
        return false;
    }

    let matrices: Vec<[f64; 6]> = operations
        .iter()
        .filter(|operation| operation.operator == "cm")
        .filter_map(|operation| {
            let values = operation
                .operands
                .iter()
                .map(pdf_number)
                .collect::<Option<Vec<_>>>()?;
            values.try_into().ok()
        })
        .collect();
    let [matrix] = matrices.as_slice() else {
        return false;
    };
    approx(matrix[0], page_width, 1.0)
        && approx(matrix[1], 0.0, 0.01)
        && approx(matrix[2], 0.0, 0.01)
        && approx(matrix[3], page_height, 1.0)
        && approx(matrix[4], 0.0, 0.1)
        && approx(matrix[5], 0.0, 0.1)
}

fn inherited_page_options_present(document: &lopdf::Document, page: &lopdf::Dictionary) -> bool {
    let mut parent = page.get(b"Parent").ok();
    for _ in 0..32 {
        let Some(lopdf::Object::Reference(id)) = parent else {
            return false;
        };
        let Ok(lopdf::Object::Dictionary(dictionary)) = document.get_object(*id) else {
            return true;
        };
        if [
            b"Rotate".as_slice(),
            b"UserUnit",
            b"CropBox",
            b"TrimBox",
            b"BleedBox",
            b"ArtBox",
        ]
        .iter()
        .any(|key| dictionary.has(key))
        {
            return true;
        }
        parent = dictionary.get(b"Parent").ok();
    }
    true
}

fn pdf_number(value: &lopdf::Object) -> Option<f64> {
    match value {
        lopdf::Object::Integer(value) => Some(*value as f64),
        lopdf::Object::Real(value) => Some(*value as f64),
        _ => None,
    }
}

fn approx(left: f64, right: f64, tolerance: f64) -> bool {
    (left - right).abs() <= tolerance
}

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

pub fn pages_from_extracted(
    pages: Vec<(EncodedPage, u8)>,
) -> Result<Vec<(ScannedPage, u8)>, String> {
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

    // Local paper map (uneven lamp/AWB across the page): per-cell bright mode,
    // floored to the global paper level so ink-dense cells cannot over-brighten.
    let mut cell_hist = vec![[[0_u64; 256]; 3]; FLATTEN_COLS * FLATTEN_ROWS];
    for y in (0..height).step_by(4) {
        let row = (y as usize * FLATTEN_ROWS / height as usize).min(FLATTEN_ROWS - 1);
        for x in (0..width).step_by(4) {
            let col = (x as usize * FLATTEN_COLS / width as usize).min(FLATTEN_COLS - 1);
            let pixel = image.get_pixel(x, y);
            let cell = &mut cell_hist[row * FLATTEN_COLS + col];
            for channel in 0..3 {
                cell[channel][pixel[channel] as usize] += 1;
            }
        }
    }
    let mut raw_bg = vec![[0.0_f32; 3]; FLATTEN_COLS * FLATTEN_ROWS];
    for (index, cell) in cell_hist.iter().enumerate() {
        for channel in 0..3 {
            let floor = papers[channel] as f32 * 0.66;
            raw_bg[index][channel] = (bright_mode(&cell[channel]) as f32).max(floor);
        }
    }
    let mut background = vec![[0.0_f32; 3]; FLATTEN_COLS * FLATTEN_ROWS];
    for row in 0..FLATTEN_ROWS {
        for col in 0..FLATTEN_COLS {
            let mut sums = [0.0_f32; 3];
            let mut count = 0.0_f32;
            for delta_row in -1_i32..=1 {
                for delta_col in -1_i32..=1 {
                    let neighbor_row = row as i32 + delta_row;
                    let neighbor_col = col as i32 + delta_col;
                    if (0..FLATTEN_ROWS as i32).contains(&neighbor_row)
                        && (0..FLATTEN_COLS as i32).contains(&neighbor_col)
                    {
                        let neighbor =
                            raw_bg[neighbor_row as usize * FLATTEN_COLS + neighbor_col as usize];
                        for channel in 0..3 {
                            sums[channel] += neighbor[channel];
                        }
                        count += 1.0;
                    }
                }
            }
            for channel in 0..3 {
                background[row * FLATTEN_COLS + col][channel] = sums[channel] / count;
            }
        }
    }
    let cell_w = width as f32 / FLATTEN_COLS as f32;
    let cell_h = height as f32 / FLATTEN_ROWS as f32;
    let sample_bg = |fx: f32, fy: f32, channel: usize| -> f32 {
        let gx = (fx / cell_w - 0.5).clamp(0.0, FLATTEN_COLS as f32 - 1.0);
        let gy = (fy / cell_h - 0.5).clamp(0.0, FLATTEN_ROWS as f32 - 1.0);
        let x0 = gx.floor() as usize;
        let y0 = gy.floor() as usize;
        let x1 = (x0 + 1).min(FLATTEN_COLS - 1);
        let y1 = (y0 + 1).min(FLATTEN_ROWS - 1);
        let tx = gx - x0 as f32;
        let ty = gy - y0 as f32;
        let top = background[y0 * FLATTEN_COLS + x0][channel] * (1.0 - tx)
            + background[y0 * FLATTEN_COLS + x1][channel] * tx;
        let bottom = background[y1 * FLATTEN_COLS + x0][channel] * (1.0 - tx)
            + background[y1 * FLATTEN_COLS + x1][channel] * tx;
        top * (1.0 - ty) + bottom * ty
    };

    // Multiplicative local white balance (keeps ink dark — QR-safe), then the
    // global paper→white / ink→dark stretch.
    let scales: [f32; 3] =
        std::array::from_fn(|channel| 242.0 / (papers[channel] - lows[channel]) as f32);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        for channel in 0..3 {
            let local_bg = sample_bg(x as f32, y as f32, channel).max(1.0);
            let factor = (papers[channel] as f32 / local_bg).clamp(0.75, 1.5);
            let balanced = pixel[channel] as f32 * factor;
            let adjusted = (balanced - lows[channel] as f32) * scales[channel] + 5.0;
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
    fn warp_canvas_follows_quad_orientation() {
        let image = RgbImage::from_pixel(400, 300, Rgb([245, 245, 245]));
        let landscape = [
            CropPoint::new(0.1, 0.2),
            CropPoint::new(0.9, 0.2),
            CropPoint::new(0.9, 0.8),
            CropPoint::new(0.1, 0.8),
        ];
        let warped = warp_only(&image, landscape, false).expect("landscape warp");
        assert_eq!(warped.dimensions(), (A4_HEIGHT_PX, A4_WIDTH_PX));

        let portrait_image = RgbImage::from_pixel(300, 400, Rgb([245, 245, 245]));
        let portrait = [
            CropPoint::new(0.2, 0.1),
            CropPoint::new(0.8, 0.1),
            CropPoint::new(0.8, 0.9),
            CropPoint::new(0.2, 0.9),
        ];
        let warped = warp_only(&portrait_image, portrait, false).expect("portrait warp");
        assert_eq!(warped.dimensions(), (A4_WIDTH_PX, A4_HEIGHT_PX));
    }

    /// Renders one raw camera frame through every enhancement generation so the
    /// versions can be compared side by side on identical input.
    /// RAW_FRAME=<in.png> OUT_DIR=<dir> cargo test --release variants -- --ignored
    #[test]
    #[ignore = "narzędzie porównawcze wersji przetwarzania"]
    fn renders_enhancement_variants() {
        let input = std::env::var("RAW_FRAME").expect("RAW_FRAME");
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
        let image = image::open(input).expect("odczyt klatki").to_rgb8();
        let detected = detect_document(&image);
        println!(
            "detect: confident={} corners={:?}",
            detected.confident, detected.corners
        );
        let warped = warp_only(&image, detected.corners, true).expect("warp");

        let mut raw = warped.clone();
        save_variant(&out_dir, "0-bez-korekcji", &raw);

        raw = warped.clone();
        enhance_v0_luminance(&mut raw);
        save_variant(&out_dir, "1-stara-wersja-luminancja", &raw);

        raw = warped.clone();
        enhance_v2_per_channel(&mut raw);
        save_variant(&out_dir, "2-kanaly-globalnie", &raw);

        raw = warped.clone();
        enhance_document(&mut raw);
        save_variant(&out_dir, "3-obecna-lokalny-balans", &raw);
    }

    fn save_variant(dir: &str, name: &str, image: &RgbImage) {
        let preview = resize_to_fit(image, 1240, 1754, imageops::FilterType::Lanczos3);
        let path = format!("{dir}/{name}.png");
        preview.save(&path).expect("zapis wariantu");
        let mut sums = [0_u64; 3];
        let mut count = 0_u64;
        for y in (preview.height() / 5..preview.height() * 4 / 5).step_by(3) {
            for x in (preview.width() / 5..preview.width() * 4 / 5).step_by(3) {
                let pixel = preview.get_pixel(x, y);
                if pixel[1] > 140 {
                    for channel in 0..3 {
                        sums[channel] += pixel[channel] as u64;
                    }
                    count += 1;
                }
            }
        }
        if count > 0 {
            println!(
                "{name}: papier RGB = {:.0} {:.0} {:.0}",
                sums[0] as f64 / count as f64,
                sums[1] as f64 / count as f64,
                sums[2] as f64 / count as f64
            );
        }
    }

    /// v0.2.1 — one luminance histogram, one scale for all channels.
    fn enhance_v0_luminance(image: &mut RgbImage) {
        let mut histogram = [0_u64; 256];
        for pixel in image.pixels().step_by(8) {
            let luminance =
                (pixel[0] as u32 * 54 + pixel[1] as u32 * 183 + pixel[2] as u32 * 19) / 256;
            histogram[luminance as usize] += 1;
        }
        let sample_count: u64 = histogram.iter().sum();
        if sample_count == 0 {
            return;
        }
        let low = percentile(&histogram, sample_count / 200);
        let high = percentile(&histogram, sample_count * 199 / 200);
        if high <= low + 60 {
            return;
        }
        let scale = 245.0 / (high - low) as f32;
        for pixel in image.pixels_mut() {
            for channel in &mut pixel.0 {
                let adjusted = (*channel as i32 - low as i32) as f32 * scale + 5.0;
                *channel = adjusted.round().clamp(0.0, 255.0) as u8;
            }
        }
    }

    /// Second generation — per-channel percentiles over the whole frame.
    fn enhance_v2_per_channel(image: &mut RgbImage) {
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
        let scales: [f32; 3] =
            std::array::from_fn(|channel| 245.0 / (highs[channel] - lows[channel]) as f32);
        for pixel in image.pixels_mut() {
            for channel in 0..3 {
                let adjusted =
                    (pixel[channel] as i32 - lows[channel] as i32) as f32 * scales[channel] + 5.0;
                pixel[channel] = adjusted.round().clamp(0.0, 255.0) as u8;
            }
        }
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
    fn extracts_pages_from_own_pdf() {
        let first = page_from_image(RgbImage::from_pixel(
            A4_WIDTH_PX,
            A4_HEIGHT_PX,
            Rgb([245, 245, 245]),
        ), ColorMode::Color)
        .expect("strona 1");
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-extract-{}.pdf",
            std::process::id()
        ));
        save_pdf(&path, &[(&first, 0)]).expect("zapis PDF");
        let pages = extract_pdf_pages(&path).expect("ekstrakcja");
        std::fs::remove_file(&path).expect("usunięcie testowego PDF");
        assert_eq!(pages.len(), 1, "oczekiwano jednej strony");
        assert_eq!(pages[0].1, 0);
        let decoded_first = image::load_from_memory(&pages[0].0.bytes).expect("dekodowanie 1");
        assert_eq!(
            (decoded_first.width(), decoded_first.height()),
            (A4_WIDTH_PX, A4_HEIGHT_PX)
        );
    }

    fn bilevel_page() -> ScannedPage {
        let mut image = RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([245, 245, 245]));
        for y in 400..440 {
            for x in 300..1800 {
                image.put_pixel(x, y, Rgb([15, 15, 15]));
            }
        }
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
        assert!(
            page.review_image
                .pixels()
                .all(|p| p.0[0] == p.0[1] && p.0[1] == p.0[2])
        );
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
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-g4-{}.pdf",
            std::process::id()
        ));
        save_pdf(&path, &[(&page, 1)]).expect("zapis PDF");
        let bytes = std::fs::read(&path).expect("odczyt PDF");
        assert!(
            bytes
                .windows(b"/CCITTFaxDecode".len())
                .any(|w| w == b"/CCITTFaxDecode")
        );
        assert!(
            bytes
                .windows(b"/BlackIs1 false".len())
                .any(|w| w == b"/BlackIs1 false")
        );
        let extracted = extract_pdf_pages(&path).expect("ekstrakcja");
        std::fs::remove_file(&path).expect("usunięcie testowego PDF");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].1, 1, "obrót z /Rotate");
        assert_eq!(
            extracted[0].0,
            page.encoded(),
            "bajty G4, kodowanie i wymiary muszą wrócić bez zmian"
        );
        let reloaded = pages_from_extracted(extracted).expect("strony");
        assert_eq!(reloaded[0].0.encoding, PageEncoding::G4);
        assert_eq!(
            (reloaded[0].0.width, reloaded[0].0.height),
            (A4_WIDTH_PX, A4_HEIGHT_PX)
        );
    }

    #[test]
    fn mixed_jpeg_and_g4_document_round_trips() {
        let colour = page_from_image(
            RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([245, 245, 245])),
            ColorMode::Color,
        )
        .expect("jpeg");
        let bilevel = bilevel_page();
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-mixed-{}.pdf",
            std::process::id()
        ));
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
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-{name}-{}.pdf",
            std::process::id()
        ));
        save_pdf(&path, &[(&page, 0)]).expect("zapis PDF");
        let mut document = lopdf::Document::load(&path).expect("load");
        let ids: Vec<lopdf::ObjectId> = document
            .objects
            .iter()
            .filter(|(_, object)| {
                matches!(object, lopdf::Object::Stream(stream) if stream.dict.has(b"DecodeParms"))
            })
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
        let g3 = save_g4_pdf_with_parms(
            "g3",
            dictionary! {
                "K" => 0_i64,
                "Columns" => i64::from(A4_WIDTH_PX),
                "Rows" => i64::from(A4_HEIGHT_PX),
                "BlackIs1" => false,
            },
        );
        assert!(
            extract_pdf_pages(&g3).is_err(),
            "K=0 (G3) must not be treated as our format"
        );
        std::fs::remove_file(g3).expect("cleanup");
        let inverted = save_g4_pdf_with_parms(
            "blackis1",
            dictionary! {
                "K" => -1_i64,
                "Columns" => i64::from(A4_WIDTH_PX),
                "Rows" => i64::from(A4_HEIGHT_PX),
                "BlackIs1" => true,
            },
        );
        assert!(
            extract_pdf_pages(&inverted).is_err(),
            "BlackIs1 true must not be treated as our format"
        );
        std::fs::remove_file(inverted).expect("cleanup");
    }

    #[test]
    fn decode_page_rejects_corrupt_g4() {
        let page = EncodedPage {
            bytes: vec![0x00, 0x01, 0x02],
            encoding: PageEncoding::G4,
            width: 64,
            height: 64,
        };
        assert!(decode_page(&page).is_err());
    }

    #[test]
    fn rotation_round_trips_as_metadata_without_touching_bytes() {
        let page = page_from_image(RgbImage::from_pixel(
            A4_WIDTH_PX,
            A4_HEIGHT_PX,
            Rgb([245, 245, 245]),
        ), ColorMode::Color)
        .expect("strona");
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-rotate-{}.pdf",
            std::process::id()
        ));
        save_pdf(&path, &[(&page, 3)]).expect("zapis PDF");
        let pages = extract_pdf_pages(&path).expect("ekstrakcja");
        std::fs::remove_file(&path).expect("usunięcie testowego PDF");
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].1, 3, "obrót musi wrócić z /Rotate");
        assert_eq!(pages[0].0.bytes, page.bytes, "bajty JPEG nie mogą się zmienić");
    }

    #[test]
    fn rotate_rgb_turns_quarters() {
        let mut image = RgbImage::from_pixel(2, 1, Rgb([10, 10, 10]));
        image.put_pixel(0, 0, Rgb([200, 0, 0]));
        let once = rotate_rgb(&image, 1);
        assert_eq!(once.dimensions(), (1, 2));
        assert_eq!(once.get_pixel(0, 0), &Rgb([200, 0, 0]));
        let full = rotate_rgb(&image, 4);
        assert_eq!(full.dimensions(), (2, 1));
        assert_eq!(full.get_pixel(0, 0), &Rgb([200, 0, 0]));
    }

    #[test]
    fn foreign_pdf_without_jpeg_streams_errors() {
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-foreign-{}.pdf",
            std::process::id()
        ));
        std::fs::write(
            &path,
            b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF",
        )
        .expect("zapis atrapy");
        let result = extract_pdf_pages(&path);
        std::fs::remove_file(&path).expect("usunięcie atrapy");
        assert!(result.is_err(), "obcy PDF musi zwrócić błąd");
    }

    #[test]
    fn editable_page_rejects_extra_content_or_multiple_images() {
        use lopdf::Object;
        use lopdf::content::Operation;

        let base = vec![
            Operation::new("q", vec![]),
            Operation::new(
                "cm",
                vec![
                    Object::Integer(1),
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(1),
                    Object::Integer(0),
                    Object::Integer(0),
                ],
            ),
            Operation::new("Do", vec![Object::Name(b"X1".to_vec())]),
            Operation::new("Q", vec![]),
        ];
        assert_eq!(editable_page_image_name(&base), Some(b"X1".as_slice()));

        let mut with_text = base.clone();
        with_text.insert(2, Operation::new("BT", vec![]));
        assert!(editable_page_image_name(&with_text).is_none());

        let mut with_second_image = base;
        with_second_image.insert(3, Operation::new("Do", vec![Object::Name(b"X2".to_vec())]));
        assert!(editable_page_image_name(&with_second_image).is_none());
    }

    #[test]
    fn flattens_uneven_illumination_keeping_ink_dark() {
        let mut image = RgbImage::new(480, 640);
        for y in 0..640 {
            for x in 0..480 {
                let base = 150.0 + 80.0 * (x as f32 / 480.0);
                image.put_pixel(
                    x,
                    y,
                    Rgb([
                        base as u8,
                        (base * 1.02) as u8,
                        (base * 1.12).min(255.0) as u8,
                    ]),
                );
            }
        }
        for y in 260..330 {
            for x in 120..360 {
                image.put_pixel(x, y, Rgb([35, 40, 60]));
            }
        }
        enhance_document(&mut image);
        for (x, y) in [
            (40_u32, 40_u32),
            (440, 40),
            (40, 600),
            (440, 600),
            (240, 470),
        ] {
            let sample = image.get_pixel(x, y);
            assert!(
                sample[0] >= 225 && sample[1] >= 225 && sample[2] >= 225,
                "tło w ({x},{y}) nie jest białe: {:?}",
                sample
            );
        }
        let ink = image.get_pixel(240, 295);
        assert!(
            ink[0] < 90 && ink[1] < 90 && ink[2] < 90,
            "atrament został rozjaśniony (QR by ucierpiał): {:?}",
            ink
        );
    }

    #[test]
    fn shadowed_page_edge_stays_inside_the_crop() {
        let mut image = RgbImage::from_pixel(800, 600, Rgb([25, 25, 25]));
        for y in 60..540 {
            for x in 100..700 {
                image.put_pixel(x, y, Rgb([240, 240, 240]));
            }
        }
        for y in 60..540 {
            for x in 100..140 {
                image.put_pixel(x, y, Rgb([105, 105, 105]));
            }
        }
        let result = detect_document(&image);
        assert!(result.confident);
        assert!(
            result.corners[0].x <= 105.0 / 800.0,
            "zacieniona krawędź wypadła z kadru: {:?}",
            result.corners[0]
        );
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
    fn pdf_page_import_is_all_or_nothing() {
        let page = page_from_image(RgbImage::from_pixel(80, 120, Rgb([240, 240, 240])), ColorMode::Color)
            .expect("valid page");
        let error = pages_from_extracted(vec![
            (page.encoded(), 0),
            (
                EncodedPage {
                    bytes: b"not-a-jpeg".to_vec(),
                    encoding: PageEncoding::Jpeg,
                    width: 80,
                    height: 120,
                },
                1,
            ),
        ])
            .err()
            .expect("second page must fail");
        assert!(error.contains("strony 2"), "unexpected error: {error}");
    }

    #[test]
    fn saved_pdf_embeds_the_exact_jpeg_bytes() {
        let image = RgbImage::from_pixel(A4_WIDTH_PX, A4_HEIGHT_PX, Rgb([245, 245, 245]));
        let page = page_from_image(image, ColorMode::Color).expect("strona testowa");
        let path =
            std::env::temp_dir().join(format!("skaner-dokumentow-res-{}.pdf", std::process::id()));
        save_pdf(&path, &[(&page, 0)]).expect("zapis PDF");
        let bytes = std::fs::read(&path).expect("odczyt PDF");
        assert!(
            bytes
                .windows(page.bytes.len())
                .any(|window| window == page.bytes.as_slice()),
            "PDF nie zawiera oryginalnych bajtów JPEG — strona została przekodowana"
        );
        let extracted = extract_pdf_pages(&path).expect("ekstrakcja");
        std::fs::remove_file(&path).expect("usunięcie testowego PDF");
        assert_eq!(extracted.len(), 1);
        assert_eq!(extracted[0].1, 0);
        assert_eq!(
            extracted[0].0.bytes, page.bytes,
            "wyodrębniona strona nie jest bajtowo identyczna z zapisaną"
        );
    }

    #[test]
    fn safely_imports_full_page_pdf_from_legacy_app_version() {
        let page = page_from_image(RgbImage::from_pixel(
            A4_WIDTH_PX,
            A4_HEIGHT_PX,
            Rgb([245, 245, 245]),
        ), ColorMode::Color)
        .expect("legacy page");
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-legacy-{}.pdf",
            std::process::id()
        ));
        save_pdf(&path, &[(&page, 0)]).expect("save marked PDF");

        let mut document = lopdf::Document::load(&path).expect("load PDF");
        let info_id = match document.trailer.get(b"Info").expect("Info") {
            lopdf::Object::Reference(id) => *id,
            _ => panic!("Info is not indirect"),
        };
        document
            .get_object_mut(info_id)
            .expect("Info object")
            .as_dict_mut()
            .expect("Info dictionary")
            .remove(b"Keywords");
        document.save(&path).expect("save legacy PDF");

        let pages = extract_pdf_pages(&path).expect("safe legacy extraction");
        assert_eq!(pages.len(), 1);
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn rejects_unmarked_pdf_that_is_not_a_full_a4_page() {
        let page = page_from_image(RgbImage::from_pixel(400, 566, Rgb([245, 245, 245])), ColorMode::Color)
            .expect("small page");
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-unsafe-legacy-{}.pdf",
            std::process::id()
        ));
        save_pdf(&path, &[(&page, 0)]).expect("save marked PDF");

        let mut document = lopdf::Document::load(&path).expect("load PDF");
        let info_id = match document.trailer.get(b"Info").expect("Info") {
            lopdf::Object::Reference(id) => *id,
            _ => panic!("Info is not indirect"),
        };
        document
            .get_object_mut(info_id)
            .expect("Info object")
            .as_dict_mut()
            .expect("Info dictionary")
            .remove(b"Keywords");
        document.save(&path).expect("save unmarked PDF");

        assert!(extract_pdf_pages(&path).is_err());
        std::fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn saving_pdf_replaces_an_existing_target() {
        let image = RgbImage::from_pixel(100, 141, Rgb([245, 245, 245]));
        let page = page_from_image(image, ColorMode::Color).expect("strona testowa");
        let path = std::env::temp_dir().join(format!(
            "skaner-dokumentow-replace-{}.pdf",
            std::process::id()
        ));
        std::fs::write(&path, b"poprzednia wersja").expect("stary plik");
        save_pdf(&path, &[(&page, 0)]).expect("podmiana PDF");
        let bytes = std::fs::read(&path).expect("odczyt PDF");
        assert!(bytes.starts_with(b"%PDF-"));
        std::fs::remove_file(path).expect("usunięcie testowego PDF");
    }

    #[test]
    fn writes_a_valid_pdf() {
        let image = RgbImage::from_pixel(100, 141, Rgb([245, 245, 245]));
        let page = page_from_image(image, ColorMode::Color).expect("strona testowa");
        let path =
            std::env::temp_dir().join(format!("skaner-dokumentow-test-{}.pdf", std::process::id()));
        save_pdf(&path, &[(&page, 0)]).expect("zapis PDF");
        let bytes = std::fs::read(&path).expect("odczyt PDF");
        assert!(bytes.starts_with(b"%PDF-"));
        std::fs::remove_file(path).expect("usunięcie testowego PDF");
    }
}
