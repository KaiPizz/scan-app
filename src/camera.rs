use image::RgbImage;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType,
};
use nokhwa::{Camera, query};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub enum CameraEvent {
    Ready {
        device_name: String,
        width: u32,
        height: u32,
    },
    Preview(RgbImage),
    Error(String),
}

pub struct CameraController {
    receiver: Receiver<CameraEvent>,
    latest_full: Arc<Mutex<Option<Arc<RgbImage>>>>,
    stop_requested: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl CameraController {
    pub fn start() -> Self {
        let (sender, receiver) = sync_channel(2);
        let latest_full = Arc::new(Mutex::new(None));
        let stop_requested = Arc::new(AtomicBool::new(false));
        let thread_image = Arc::clone(&latest_full);
        let thread_stop = Arc::clone(&stop_requested);
        let worker = thread::spawn(move || {
            if let Err(error) = run_camera(&sender, &thread_image, &thread_stop) {
                let _ = sender.send(CameraEvent::Error(error));
            }
        });
        Self {
            receiver,
            latest_full,
            stop_requested,
            worker: Some(worker),
        }
    }

    pub fn try_event(&self) -> Option<CameraEvent> {
        self.receiver.try_recv().ok()
    }

    pub fn latest_full_image(&self) -> Option<Arc<RgbImage>> {
        self.latest_full.lock().ok()?.clone()
    }

    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for CameraController {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_camera(
    sender: &SyncSender<CameraEvent>,
    latest_full: &Arc<Mutex<Option<Arc<RgbImage>>>>,
    stop_requested: &Arc<AtomicBool>,
) -> Result<(), String> {
    let mut camera_info = None;
    let mut available_names = Vec::new();
    for attempt in 0..8 {
        let cameras = query(ApiBackend::MediaFoundation)
            .map_err(|error| format!("Nie można odczytać listy kamer: {error}"))?;
        available_names = cameras
            .iter()
            .map(|camera| camera.human_name().to_owned())
            .collect();
        camera_info = cameras.into_iter().find(|camera| {
            let name = camera.human_name().to_lowercase();
            name.contains("iriscan") || name.contains("visualizer 7")
        });
        if camera_info.is_some() {
            break;
        }
        if stop_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        if attempt < 7 {
            thread::sleep(Duration::from_millis(650));
        }
    }
    let camera_info = camera_info.ok_or_else(|| {
        if available_names.is_empty() {
            "Nie znaleziono żadnej kamery. Podłącz IRIScan Visualizer 7.".to_owned()
        } else {
            format!(
                "Nie znaleziono IRIScan Visualizer 7. Dostępne kamery: {}",
                available_names.join(", ")
            )
        }
    })?;

    let device_name = camera_info.human_name().to_owned();
    let (mut camera, first_frame) = open_best_stream(camera_info.index())?;
    let resolution = camera.resolution();
    let _ = sender.send(CameraEvent::Ready {
        device_name,
        width: resolution.width(),
        height: resolution.height(),
    });
    publish_frame(sender, latest_full, first_frame);

    while !stop_requested.load(Ordering::Acquire) {
        let buffer = camera
            .frame()
            .map_err(|error| friendly_stream_error(&error.to_string()))?;
        let decoded = buffer
            .decode_image::<RgbFormat>()
            .map_err(|error| format!("Nie można odczytać obrazu z kamery: {error}"))?;
        publish_frame(sender, latest_full, decoded);
        thread::sleep(Duration::from_millis(70));
    }
    let _ = camera.stop_stream();
    Ok(())
}

fn open_best_stream(index: &CameraIndex) -> Result<(Camera, RgbImage), String> {
    let probe_request = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
    let mut probe = Camera::with_backend(index.clone(), probe_request, ApiBackend::MediaFoundation)
        .map_err(|error| friendly_stream_error(&error.to_string()))?;
    let supported_formats = probe.compatible_camera_formats().unwrap_or_default();
    drop(probe);

    let candidates = preferred_formats(supported_formats);
    let mut failures = Vec::new();
    for format in candidates {
        let request = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(format));
        let mut camera =
            match Camera::with_backend(index.clone(), request, ApiBackend::MediaFoundation) {
                Ok(camera) => camera,
                Err(error) => {
                    failures.push(format!("{format}: {error}"));
                    continue;
                }
            };
        if let Err(error) = camera.open_stream() {
            failures.push(format!("{format}: {error}"));
            continue;
        }
        match camera
            .frame()
            .and_then(|buffer| buffer.decode_image::<RgbFormat>())
        {
            Ok(frame) => return Ok((camera, frame)),
            Err(error) => {
                failures.push(format!("{format}: {error}"));
                let _ = camera.stop_stream();
            }
        }
    }

    let details = failures.last().cloned().unwrap_or_default();
    Err(friendly_stream_error(&details))
}

fn preferred_formats(mut formats: Vec<CameraFormat>) -> Vec<CameraFormat> {
    formats.retain(|format| {
        format.format() == FrameFormat::MJPEG
            && format.resolution().width() >= 1280
            && format.resolution().height() >= 720
            && format.frame_rate() <= 30
    });
    formats.sort_by(|left, right| {
        let left_pixels = left.resolution().width() as u64 * left.resolution().height() as u64;
        let right_pixels = right.resolution().width() as u64 * right.resolution().height() as u64;
        right_pixels
            .cmp(&left_pixels)
            .then_with(|| right.frame_rate().cmp(&left.frame_rate()))
    });

    let mut resolutions = HashSet::new();
    let mut candidates = formats
        .into_iter()
        .filter(|format| resolutions.insert(format.resolution()))
        .take(8)
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        candidates = vec![
            CameraFormat::new_from(4160, 3120, FrameFormat::MJPEG, 30),
            CameraFormat::new_from(3840, 2160, FrameFormat::MJPEG, 30),
            CameraFormat::new_from(1920, 1080, FrameFormat::MJPEG, 30),
            CameraFormat::new_from(1280, 720, FrameFormat::MJPEG, 30),
        ];
    }
    candidates
}

fn publish_frame(
    sender: &SyncSender<CameraEvent>,
    latest_full: &Arc<Mutex<Option<Arc<RgbImage>>>>,
    frame: RgbImage,
) {
    let frame = Arc::new(frame);
    let preview = image::imageops::thumbnail(frame.as_ref(), 1280, 960);
    if let Ok(mut latest) = latest_full.lock() {
        *latest = Some(Arc::clone(&frame));
    }
    match sender.try_send(CameraEvent::Preview(preview)) {
        Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
    }
}

fn friendly_stream_error(details: &str) -> String {
    if details.contains("0xC00D3704")
        || details.to_lowercase().contains("braku zasobów sprzętowych")
    {
        return "IRIScan jest używany przez inny program. Zamknij Readiris Visual, Teams, Zoom lub aplikację Aparat, a następnie spróbuj ponownie.".to_owned();
    }
    if details.is_empty() {
        "Nie udało się uruchomić obrazu z IRIScan Visualizer 7.".to_owned()
    } else {
        format!("Nie udało się uruchomić obrazu z IRIScan Visualizer 7: {details}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    #[ignore = "wymaga podłączonego IRIScan Visualizer 7"]
    fn connected_iriscan_produces_preview() {
        let mut controller = CameraController::start();
        let deadline = Instant::now() + Duration::from_secs(25);
        let mut ready = None;
        let mut preview_size = None;

        while Instant::now() < deadline && (ready.is_none() || preview_size.is_none()) {
            while let Some(event) = controller.try_event() {
                match event {
                    CameraEvent::Ready {
                        device_name,
                        width,
                        height,
                    } => ready = Some((device_name, width, height)),
                    CameraEvent::Preview(image) => {
                        preview_size = Some((image.width(), image.height()));
                    }
                    CameraEvent::Error(error) => panic!("Błąd kamery: {error}"),
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
        let full_image = controller.latest_full_image();
        controller.stop();

        if let (Some(image), Ok(path)) = (full_image, std::env::var("IRISCAN_TEST_FRAME")) {
            image.save(path).expect("zapis testowej klatki");
        }

        let (device_name, width, height) = ready.expect("Kamera nie zgłosiła gotowości");
        let (preview_width, preview_height) = preview_size.expect("Kamera nie wysłała podglądu");
        println!("{device_name}: {width}x{height}, podgląd {preview_width}x{preview_height}");
        assert!(device_name.to_lowercase().contains("iriscan"));
        assert!(width >= 1920 && height >= 1080);
        assert!(preview_width > 0 && preview_height > 0);
    }
}
