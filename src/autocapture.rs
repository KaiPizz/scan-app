use image::{RgbImage, imageops};
use std::time::{Duration, Instant};

pub const MOTION_MAX: f32 = 2.5;
pub const NOVELTY_MIN: f32 = 12.0;
pub const STABLE_MS: u64 = 700;
const SAMPLE_WIDTH: u32 = 160;
const SAMPLE_HEIGHT: u32 = 120;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Baseline,
    Armed,
    Settling,
    Cooldown,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FeedResult {
    None,
    Trigger,
}

pub struct AutoCapture {
    enabled: bool,
    state: State,
    previous: Option<Vec<u8>>,
    captured: Option<Vec<u8>>,
    stable_since: Option<Instant>,
}

impl AutoCapture {
    pub fn new() -> Self {
        Self {
            enabled: true,
            state: State::Baseline,
            previous: None,
            captured: None,
            stable_since: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, on: bool) {
        if on && !self.enabled {
            self.state = State::Baseline;
            self.captured = None;
            self.stable_since = None;
        }
        self.enabled = on;
    }

    pub fn note_manual_capture(&mut self) {
        if let Some(previous) = &self.previous {
            self.captured = Some(previous.clone());
        }
        self.state = State::Cooldown;
        self.stable_since = None;
    }

    pub fn hint(&self) -> &'static str {
        if !self.enabled {
            return "Auto wyłączone — Spacja robi zdjęcie";
        }
        match self.state {
            State::Baseline => "Połóż stronę pod kamerą",
            State::Armed => "Połóż następną stronę",
            State::Settling => "Trzymaj nieruchomo…",
            State::Cooldown => "Zmień stronę",
        }
    }

    pub fn settle_progress(&self, now: Instant) -> f32 {
        match (self.state, self.stable_since) {
            (State::Settling, Some(since)) => {
                (now.duration_since(since).as_millis() as f32 / STABLE_MS as f32).clamp(0.0, 1.0)
            }
            _ => 0.0,
        }
    }

    pub fn feed(&mut self, preview: &RgbImage, now: Instant, document_present: bool) -> FeedResult {
        let sample = downsample(preview);
        let motion = self
            .previous
            .as_ref()
            .map(|previous| mean_abs_diff(previous, &sample));
        let novelty = self
            .captured
            .as_ref()
            .map(|captured| mean_abs_diff(captured, &sample));
        self.previous = Some(sample.clone());
        if !self.enabled {
            return FeedResult::None;
        }
        match self.state {
            State::Baseline => {
                self.captured = Some(sample);
                self.state = State::Cooldown;
                FeedResult::None
            }
            State::Armed => {
                if document_present && novelty.unwrap_or(f32::INFINITY) > NOVELTY_MIN {
                    self.state = State::Settling;
                    self.stable_since = Some(now);
                }
                FeedResult::None
            }
            State::Settling => {
                if !document_present || novelty.unwrap_or(f32::INFINITY) <= NOVELTY_MIN {
                    self.state = State::Armed;
                    self.stable_since = None;
                    return FeedResult::None;
                }
                if motion.unwrap_or(f32::INFINITY) > MOTION_MAX {
                    self.stable_since = Some(now);
                    return FeedResult::None;
                }
                let stable_since = *self.stable_since.get_or_insert(now);
                if now.duration_since(stable_since) >= Duration::from_millis(STABLE_MS) {
                    self.captured = Some(sample);
                    self.state = State::Cooldown;
                    self.stable_since = None;
                    FeedResult::Trigger
                } else {
                    FeedResult::None
                }
            }
            State::Cooldown => {
                if motion.unwrap_or(0.0) > MOTION_MAX || novelty.unwrap_or(0.0) > NOVELTY_MIN {
                    self.state = State::Armed;
                }
                FeedResult::None
            }
        }
    }
}

fn downsample(image: &RgbImage) -> Vec<u8> {
    let small = imageops::thumbnail(image, SAMPLE_WIDTH, SAMPLE_HEIGHT);
    small
        .pixels()
        .map(|pixel| {
            ((pixel[0] as u32 * 54 + pixel[1] as u32 * 183 + pixel[2] as u32 * 19) / 256) as u8
        })
        .collect()
}

fn mean_abs_diff(left: &[u8], right: &[u8]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return f32::INFINITY;
    }
    let total: u64 = left
        .iter()
        .zip(right.iter())
        .map(|(a, b)| a.abs_diff(*b) as u64)
        .sum();
    total as f32 / left.len() as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    fn frame(level: u8) -> RgbImage {
        RgbImage::from_pixel(32, 24, Rgb([level, level, level]))
    }

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn does_not_trigger_on_startup_scene() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        for step in 0..30 {
            assert_eq!(
                auto.feed(&frame(200), t(base, step * 100), true),
                FeedResult::None,
                "pusty blat nie może wyzwolić zdjęcia"
            );
        }
    }

    #[test]
    fn triggers_after_page_settles() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0), true);
        assert_eq!(auto.feed(&frame(100), t(base, 100), true), FeedResult::None);
        assert_eq!(auto.feed(&frame(100), t(base, 200), true), FeedResult::None);
        assert_eq!(auto.feed(&frame(100), t(base, 900), true), FeedResult::Trigger);
        for step in 10..20 {
            assert_eq!(
                auto.feed(&frame(100), t(base, step * 100), true),
                FeedResult::None,
                "ta sama strona nie może wyzwolić drugi raz"
            );
        }
    }

    #[test]
    fn motion_resets_the_settle_timer() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0), true);
        auto.feed(&frame(100), t(base, 100), true);
        auto.feed(&frame(100), t(base, 200), true);
        assert_eq!(auto.feed(&frame(110), t(base, 500), true), FeedResult::None);
        assert_eq!(auto.feed(&frame(100), t(base, 900), true), FeedResult::None);
        assert_eq!(auto.feed(&frame(100), t(base, 1600), true), FeedResult::Trigger);
    }

    #[test]
    fn flip_produces_second_trigger() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0), true);
        auto.feed(&frame(100), t(base, 100), true);
        auto.feed(&frame(100), t(base, 200), true);
        assert_eq!(auto.feed(&frame(100), t(base, 900), true), FeedResult::Trigger);
        auto.feed(&frame(200), t(base, 1000), true);
        auto.feed(&frame(50), t(base, 1100), true);
        assert_eq!(auto.feed(&frame(50), t(base, 1200), true), FeedResult::None);
        assert_eq!(auto.feed(&frame(50), t(base, 1900), true), FeedResult::Trigger);
    }

    #[test]
    fn disabled_never_triggers_and_reenable_rebaselines() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0), true);
        auto.set_enabled(false);
        auto.feed(&frame(100), t(base, 100), true);
        assert_eq!(auto.feed(&frame(100), t(base, 900), true), FeedResult::None);
        auto.set_enabled(true);
        assert_eq!(auto.feed(&frame(100), t(base, 1000), true), FeedResult::None);
        for step in 11..18 {
            assert_eq!(
                auto.feed(&frame(100), t(base, step * 100), true),
                FeedResult::None,
                "po ponownym włączeniu bieżąca scena nie wyzwala"
            );
        }
        auto.feed(&frame(50), t(base, 1900), true);
        auto.feed(&frame(50), t(base, 2000), true);
        assert_eq!(auto.feed(&frame(50), t(base, 2700), true), FeedResult::Trigger);
    }

    #[test]
    fn no_document_no_trigger() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0), true);
        auto.feed(&frame(100), t(base, 100), false);
        auto.feed(&frame(100), t(base, 200), false);
        assert_eq!(
            auto.feed(&frame(100), t(base, 900), false),
            FeedResult::None,
            "bez wykrytego dokumentu nie wolno wyzwolić"
        );
        auto.feed(&frame(100), t(base, 1000), true);
        assert_eq!(auto.feed(&frame(100), t(base, 1700), true), FeedResult::Trigger);
    }

    #[test]
    fn manual_capture_enters_cooldown() {
        let mut auto = AutoCapture::new();
        let base = Instant::now();
        auto.feed(&frame(200), t(base, 0), true);
        auto.feed(&frame(100), t(base, 100), true);
        auto.note_manual_capture();
        for step in 2..12 {
            assert_eq!(
                auto.feed(&frame(100), t(base, step * 100), true),
                FeedResult::None,
                "po ręcznym zdjęciu ta sama strona nie wyzwala auto"
            );
        }
    }
}
