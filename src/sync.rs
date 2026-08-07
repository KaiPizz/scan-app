use std::sync::mpsc::Sender;
use std::time::Duration;

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

/// Upload a processed page in a background thread. The outcome lands on the
/// channel; sending is best-effort in case the app already shut down.
pub fn spawn_upload(
    backend_url: String,
    salon_id: String,
    api_key: String,
    page_id: u64,
    file_name: String,
    jpeg: Vec<u8>,
    tx: Sender<SyncOutcome>,
) {
    std::thread::spawn(move || {
        let result = upload_scan(&backend_url, &salon_id, &api_key, &file_name, jpeg);
        let _ = tx.send(SyncOutcome { page_id, result });
    });
}

fn upload_scan(
    backend_url: &str,
    salon_id: &str,
    api_key: &str,
    file_name: &str,
    jpeg: Vec<u8>,
) -> Result<String, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?;
    let part = multipart::Part::bytes(jpeg)
        .file_name(file_name.to_owned())
        .mime_str("image/jpeg")
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
