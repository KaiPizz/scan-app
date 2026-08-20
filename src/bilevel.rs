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
}
