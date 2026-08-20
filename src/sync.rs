use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::document::{EncodedPage, PageEncoding};
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use reqwest::blocking::{Client, multipart};
use serde::Deserialize;

/// Result of one page upload, reported back to the UI thread.
#[derive(Debug)]
pub struct SyncOutcome {
    pub page_id: u64,
    pub result: Result<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanResponse {
    processed_url: String,
}

/// Bytes + MIME + extension the backend accepts. The backend (sharp) only
/// takes jpeg/png/webp, so a G4 page is shipped as an 8-bit grayscale PNG
/// (two-valued, so deflate keeps it small). `image` 0.25 cannot write 1-bit PNG.
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

/// Upload a processed page in a background thread. The outcome lands on the
/// channel; sending is best-effort in case the app already shut down.
pub fn spawn_upload(
    backend_url: String,
    salon_id: String,
    api_key: String,
    page_id: u64,
    page: EncodedPage,
    tx: Sender<SyncOutcome>,
) {
    std::thread::spawn(move || {
        let result = upload_payload(&page).and_then(|(bytes, mime, ext)| {
            upload_scan(
                &backend_url,
                &salon_id,
                &api_key,
                &format!("scan-{page_id}.{ext}"),
                mime,
                bytes,
            )
        });
        let _ = tx.send(SyncOutcome { page_id, result });
    });
}

fn upload_scan(
    backend_url: &str,
    salon_id: &str,
    api_key: &str,
    file_name: &str,
    mime: &str,
    bytes: Vec<u8>,
) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?;
    let part = multipart::Part::bytes(bytes)
        .file_name(file_name.to_owned())
        .mime_str(mime)
        .map_err(|error| error.to_string())?;
    let form = multipart::Form::new().part("file", part);
    let url = format!("{}/media/scan/process", backend_url.trim_end_matches('/'));
    let response = client
        .post(url)
        .query(&[("salonId", salon_id)])
        .header("x-scan-key", api_key)
        .multipart(form)
        .send()
        .map_err(|error| format!("Połączenie nie powiodło się: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        let snippet: String = body.chars().take(200).collect();
        return Err(format!("Serwer odrzucił stronę ({status}): {snippet}"));
    }
    response
        .json::<ScanResponse>()
        .map(|parsed| parsed.processed_url)
        .map_err(|error| format!("Niepoprawna odpowiedź serwera: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jpeg_pages_upload_as_is() {
        let page = EncodedPage {
            bytes: vec![0xFF, 0xD8, 0xFF, 0xD9],
            encoding: PageEncoding::Jpeg,
            width: 1,
            height: 1,
        };
        let (bytes, mime, ext) = upload_payload(&page).expect("payload");
        assert_eq!(bytes, page.bytes);
        assert_eq!((mime, ext), ("image/jpeg", "jpg"));
    }

    #[test]
    fn g4_pages_upload_as_png_the_backend_can_decode() {
        let mut image = image::GrayImage::from_pixel(64, 32, image::Luma([255]));
        for x in 10..20 {
            image.put_pixel(x, 5, image::Luma([0]));
        }
        let page = EncodedPage {
            bytes: crate::bilevel::encode_g4(&image),
            encoding: PageEncoding::G4,
            width: 64,
            height: 32,
        };
        let (bytes, mime, ext) = upload_payload(&page).expect("payload");
        assert_eq!((mime, ext), ("image/png", "png"));
        let decoded = image::load_from_memory(&bytes).expect("png").to_luma8();
        assert_eq!(decoded.dimensions(), (64, 32));
        assert_eq!(decoded.get_pixel(15, 5), &image::Luma([0]));
        assert_eq!(decoded.get_pixel(40, 20), &image::Luma([255]));
    }
}
