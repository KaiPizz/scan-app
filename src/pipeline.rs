use crate::document::{CropPoint, ScannedPage, detect_document_corners, process_page};
use image::RgbImage;
use image::codecs::jpeg::JpegEncoder;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::{self, JoinHandle};

pub const QUEUE_CAPACITY: usize = 8;
const ORIGINAL_JPEG_QUALITY: u8 = 88;

pub enum PipelineEvent {
    PageReady {
        id: u64,
        page: ScannedPage,
        original_jpeg: Vec<u8>,
        corners: [CropPoint; 4],
    },
    PageFailed {
        id: u64,
        original_jpeg: Vec<u8>,
        error: String,
    },
    ReprocessDone {
        id: u64,
        page: ScannedPage,
        corners: [CropPoint; 4],
    },
    ReprocessFailed {
        id: u64,
        error: String,
    },
}

enum Job {
    New {
        id: u64,
        frame: Arc<RgbImage>,
    },
    Reprocess {
        id: u64,
        frame: Arc<RgbImage>,
        corners: [CropPoint; 4],
    },
}

pub struct ProcessingPipeline {
    jobs: Option<SyncSender<Job>>,
    events: Receiver<PipelineEvent>,
    worker: Option<JoinHandle<()>>,
    abort: Arc<AtomicBool>,
}

impl ProcessingPipeline {
    pub fn start() -> Self {
        let (job_sender, job_receiver) = sync_channel::<Job>(QUEUE_CAPACITY);
        let (event_sender, event_receiver) = channel();
        let abort = Arc::new(AtomicBool::new(false));
        let worker_abort = Arc::clone(&abort);
        let worker =
            thread::spawn(move || worker_loop(&job_receiver, &event_sender, &worker_abort));
        Self {
            jobs: Some(job_sender),
            events: event_receiver,
            worker: Some(worker),
            abort,
        }
    }

    pub fn try_submit(&self, id: u64, frame: Arc<RgbImage>) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.try_send(Job::New { id, frame }).is_ok())
    }

    pub fn submit_reprocess(&self, id: u64, frame: Arc<RgbImage>, corners: [CropPoint; 4]) -> bool {
        self.jobs.as_ref().is_some_and(|jobs| {
            jobs.try_send(Job::Reprocess { id, frame, corners }).is_ok()
        })
    }

    pub fn try_event(&self) -> Option<PipelineEvent> {
        self.events.try_recv().ok()
    }

    pub fn shutdown(&mut self) {
        self.abort.store(true, Ordering::Release);
        self.jobs = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ProcessingPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(jobs: &Receiver<Job>, events: &Sender<PipelineEvent>, abort: &Arc<AtomicBool>) {
    while let Ok(job) = jobs.recv() {
        if abort.load(Ordering::Acquire) {
            return;
        }
        if events.send(process_job(&job)).is_err() {
            return;
        }
    }
}

fn process_job(job: &Job) -> PipelineEvent {
    match job {
        Job::New { id, frame } => {
            let corners = detect_document_corners(frame);
            let mut original_jpeg = Vec::new();
            if JpegEncoder::new_with_quality(&mut original_jpeg, ORIGINAL_JPEG_QUALITY)
                .encode_image(frame.as_ref())
                .is_err()
            {
                original_jpeg.clear();
            }
            match process_page(frame, corners) {
                Ok(page) => PipelineEvent::PageReady {
                    id: *id,
                    page,
                    original_jpeg,
                    corners,
                },
                Err(error) => PipelineEvent::PageFailed {
                    id: *id,
                    original_jpeg,
                    error,
                },
            }
        }
        Job::Reprocess { id, frame, corners } => match process_page(frame, *corners) {
            Ok(page) => PipelineEvent::ReprocessDone {
                id: *id,
                page,
                corners: *corners,
            },
            Err(error) => PipelineEvent::ReprocessFailed {
                id: *id,
                error,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{A4_HEIGHT_PX, A4_WIDTH_PX};
    use image::Rgb;
    use std::time::{Duration, Instant};

    fn white_document_frame(width: u32, height: u32) -> RgbImage {
        let mut image = RgbImage::from_pixel(width, height, Rgb([25, 25, 25]));
        let margin_x = width / 5;
        let margin_y = height / 5;
        for y in margin_y..height - margin_y {
            for x in margin_x..width - margin_x {
                image.put_pixel(x, y, Rgb([245, 245, 245]));
            }
        }
        image
    }

    fn collect_events(
        pipeline: &ProcessingPipeline,
        count: usize,
        timeout: Duration,
    ) -> Vec<PipelineEvent> {
        let deadline = Instant::now() + timeout;
        let mut events = Vec::new();
        while events.len() < count && Instant::now() < deadline {
            match pipeline.try_event() {
                Some(event) => events.push(event),
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
        events
    }

    #[test]
    fn processes_pages_in_submit_order_with_caller_ids() {
        let pipeline = ProcessingPipeline::start();
        assert!(pipeline.try_submit(7, Arc::new(white_document_frame(400, 300))));
        assert!(pipeline.try_submit(9, Arc::new(white_document_frame(300, 400))));
        let events = collect_events(&pipeline, 2, Duration::from_secs(120));
        assert_eq!(events.len(), 2, "worker nie odesłał dwóch zdarzeń");
        let ids: Vec<u64> = events
            .iter()
            .map(|event| match event {
                PipelineEvent::PageReady { id, .. } => *id,
                PipelineEvent::PageFailed { id, .. } => *id,
                PipelineEvent::ReprocessDone { id, .. } => *id,
                PipelineEvent::ReprocessFailed { id, .. } => *id,
            })
            .collect();
        assert_eq!(ids, vec![7, 9]);
        for event in events {
            match event {
                PipelineEvent::PageReady {
                    page,
                    original_jpeg,
                    corners,
                    ..
                } => {
                    assert_eq!((page.width, page.height), (A4_WIDTH_PX, A4_HEIGHT_PX));
                    assert!(
                        original_jpeg.starts_with(&[0xFF, 0xD8]),
                        "oryginał nie jest JPEG"
                    );
                    for corner in corners {
                        assert!((0.0..=1.0).contains(&corner.x));
                        assert!((0.0..=1.0).contains(&corner.y));
                    }
                }
                PipelineEvent::PageFailed { error, .. } => {
                    panic!("nieoczekiwany błąd przetwarzania: {error}")
                }
                _ => panic!("nieoczekiwany rodzaj zdarzenia"),
            }
        }
    }

    #[test]
    fn reprocess_uses_caller_corners() {
        let pipeline = ProcessingPipeline::start();
        let corners = [
            CropPoint::new(0.2, 0.2),
            CropPoint::new(0.8, 0.2),
            CropPoint::new(0.8, 0.8),
            CropPoint::new(0.2, 0.8),
        ];
        assert!(pipeline.submit_reprocess(3, Arc::new(white_document_frame(400, 300)), corners));
        let events = collect_events(&pipeline, 1, Duration::from_secs(120));
        match events.first() {
            Some(PipelineEvent::ReprocessDone {
                id,
                page,
                corners: returned,
            }) => {
                assert_eq!(*id, 3);
                assert_eq!((page.width, page.height), (A4_WIDTH_PX, A4_HEIGHT_PX));
                assert_eq!(returned, &corners);
            }
            _ => panic!("oczekiwano ReprocessDone"),
        }
    }

    #[test]
    fn shutdown_aborts_queued_jobs_promptly() {
        let mut pipeline = ProcessingPipeline::start();
        for id in 0..6 {
            let _ = pipeline.try_submit(id, Arc::new(white_document_frame(400, 300)));
        }
        let started = Instant::now();
        pipeline.shutdown();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "shutdown czekał na całą kolejkę zamiast ją porzucić"
        );
    }
}
