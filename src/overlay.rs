use crate::document::{DetectResult, detect_document};
use image::RgbImage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const DETECT_INTERVAL_MS: u64 = 330;

pub struct OverlayDetector {
    input: Arc<Mutex<Option<RgbImage>>>,
    output: Arc<Mutex<Option<DetectResult>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl OverlayDetector {
    pub fn start() -> Self {
        let input: Arc<Mutex<Option<RgbImage>>> = Arc::new(Mutex::new(None));
        let output: Arc<Mutex<Option<DetectResult>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_input = Arc::clone(&input);
        let worker_output = Arc::clone(&output);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                let frame = worker_input.lock().ok().and_then(|mut slot| slot.take());
                if let Some(frame) = frame {
                    let result = detect_document(&frame);
                    if let Ok(mut slot) = worker_output.lock() {
                        *slot = Some(result);
                    }
                }
                thread::sleep(Duration::from_millis(DETECT_INTERVAL_MS));
            }
        });
        Self {
            input,
            output,
            stop,
            worker: Some(worker),
        }
    }

    pub fn submit(&self, frame: RgbImage) {
        if let Ok(mut slot) = self.input.lock() {
            *slot = Some(frame);
        }
    }

    pub fn latest(&self) -> Option<DetectResult> {
        self.output.lock().ok().and_then(|slot| *slot)
    }

    /// A finished worker before `stop` was requested means the detector
    /// thread panicked; the caller should start a fresh one.
    pub fn is_dead(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self
                .worker
                .as_ref()
                .is_some_and(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for OverlayDetector {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
