use crate::document::EncodedPage;
use eframe::egui::{
    self, Color32, ColorImage, CursorIcon, Pos2, Rect, Sense, Stroke, TextureHandle,
    TextureOptions, Ui, Vec2,
};
use std::sync::mpsc::{Receiver, Sender, channel};

const MIN_ZOOM: f32 = 0.01;
const MAX_ZOOM: f32 = 4.0;
const ZOOM_STEP: f32 = 1.25;

struct DecodeResult {
    key: PageTextureKey,
    outcome: Result<(TextureHandle, Vec2), String>,
}

struct DecodeJob {
    key: PageTextureKey,
    page: EncodedPage,
    max_texture_side: usize,
}

/// One long-lived decode thread; queued jobs collapse to the newest one, so
/// holding the arrow key through a stack never piles up 11 MP decodes.
fn decode_worker(
    context: egui::Context,
    jobs: Receiver<DecodeJob>,
    results: Sender<DecodeResult>,
) {
    while let Ok(mut job) = jobs.recv() {
        while let Ok(newer) = jobs.try_recv() {
            job = newer;
        }
        let outcome = decode_full_texture(&context, job.key, &job.page, job.max_texture_side);
        if results
            .send(DecodeResult {
                key: job.key,
                outcome,
            })
            .is_err()
        {
            return;
        }
        context.request_repaint();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageTextureKey {
    pub id: u64,
    pub revision: u64,
    pub quarter_turns: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoomMode {
    Fit,
    Manual,
}

#[derive(Clone, Copy, Debug)]
enum ViewCommand {
    Fit,
    OneToOne,
    ZoomBy(f32),
}

pub struct ReviewViewport {
    key: Option<PageTextureKey>,
    texture: Option<TextureHandle>,
    /// Small strip thumbnail shown at full layout size until the background
    /// decode of the real page finishes.
    placeholder: Option<TextureHandle>,
    source_px: Vec2,
    zoom: f32,
    pan: Vec2,
    mode: ZoomMode,
    pending_command: Option<ViewCommand>,
    load_error: Option<String>,
    decode_tx: Sender<DecodeResult>,
    decode_rx: Receiver<DecodeResult>,
    /// Lazily started on the first page (needs a `Context`); dropping the
    /// sender ends the worker.
    job_tx: Option<Sender<DecodeJob>>,
}

impl Default for ReviewViewport {
    fn default() -> Self {
        let (decode_tx, decode_rx) = channel();
        Self {
            key: None,
            texture: None,
            placeholder: None,
            source_px: Vec2::ZERO,
            zoom: 1.0,
            pan: Vec2::ZERO,
            mode: ZoomMode::Fit,
            pending_command: None,
            load_error: None,
            decode_tx,
            decode_rx,
            job_tx: None,
        }
    }
}

impl ReviewViewport {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn invalidate(&mut self) {
        self.key = None;
        self.texture = None;
        self.placeholder = None;
        self.source_px = Vec2::ZERO;
        self.mode = ZoomMode::Fit;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.pending_command = None;
        self.load_error = None;
    }

    /// Switches the viewport to a page. The heavy page decode happens on a
    /// background thread; until it lands, the strip thumbnail (if provided)
    /// is drawn at the final layout size, so navigation never blocks.
    pub fn ensure_page(
        &mut self,
        context: &egui::Context,
        key: PageTextureKey,
        page: &EncodedPage,
        placeholder: Option<TextureHandle>,
    ) {
        if self.key == Some(key) {
            return;
        }
        let page_px = Vec2::new(page.width as f32, page.height as f32);
        self.key = Some(key);
        self.texture = None;
        self.placeholder = placeholder;
        // Layout coordinates are known up front from the page dimensions, so
        // fit/zoom math is identical before and after the decode lands.
        self.source_px = if key.quarter_turns % 2 == 1 {
            Vec2::new(page_px.y, page_px.x)
        } else {
            page_px
        };
        self.mode = ZoomMode::Fit;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.pending_command = None;
        self.load_error = None;

        let max_texture_side = context.input(|input| input.max_texture_side);
        let job_tx = self.job_tx.get_or_insert_with(|| {
            let (job_tx, job_rx) = channel();
            let context = context.clone();
            let results = self.decode_tx.clone();
            std::thread::Builder::new()
                .name("review-decode".to_owned())
                .spawn(move || decode_worker(context, job_rx, results))
                .expect("cannot start review decode thread");
            job_tx
        });
        let _ = job_tx.send(DecodeJob {
            key,
            page: page.clone(),
            max_texture_side,
        });
    }

    fn poll_decoded(&mut self) {
        while let Ok(result) = self.decode_rx.try_recv() {
            if self.key != Some(result.key) {
                continue;
            }
            match result.outcome {
                Ok((texture, source_px)) => {
                    self.texture = Some(texture);
                    self.source_px = source_px;
                }
                Err(error) => self.load_error = Some(error),
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui) {
        self.poll_decoded();
        self.toolbar(ui);
        ui.add_space(6.0);

        let canvas_size = ui.available_size().max(Vec2::splat(1.0));
        let (response, painter) = ui.allocate_painter(canvas_size, Sense::click_and_drag());
        let viewport = response.rect;
        let painter = painter.with_clip_rect(viewport);
        painter.rect_filled(viewport, 10.0, Color32::from_rgb(28, 31, 36));

        let Some(texture) = self.texture.as_ref().or(self.placeholder.as_ref()) else {
            self.pending_command = None;
            let text = self
                .load_error
                .as_deref()
                .unwrap_or("Przygotowywanie pełnego podglądu…");
            painter.text(
                viewport.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional(16.0),
                Color32::from_gray(225),
            );
            return;
        };

        let pixels_per_point = ui.ctx().pixels_per_point().max(0.01);
        let natural_size = natural_size_points(self.source_px, pixels_per_point);
        let fit = fit_zoom(self.source_px, viewport.size(), pixels_per_point);
        if self.mode == ZoomMode::Fit {
            self.zoom = fit;
            self.pan = Vec2::ZERO;
        }
        if let Some(command) = self.pending_command.take() {
            match command {
                ViewCommand::Fit => {
                    self.mode = ZoomMode::Fit;
                    self.zoom = fit;
                    self.pan = Vec2::ZERO;
                }
                ViewCommand::OneToOne => {
                    self.mode = ZoomMode::Manual;
                    self.zoom = 1.0_f32.clamp(fit, MAX_ZOOM);
                    self.pan = Vec2::ZERO;
                }
                ViewCommand::ZoomBy(factor) => {
                    let next = (self.zoom * factor).clamp(fit, MAX_ZOOM);
                    if next <= fit + f32::EPSILON {
                        self.mode = ZoomMode::Fit;
                        self.zoom = fit;
                        self.pan = Vec2::ZERO;
                    } else if (next - self.zoom).abs() > f32::EPSILON {
                        self.pan = zoom_pan_anchored(
                            self.pan,
                            self.zoom,
                            next,
                            viewport.center(),
                            viewport.center(),
                        );
                        self.zoom = next;
                        self.mode = ZoomMode::Manual;
                    }
                }
            }
        }

        if response.dragged_by(egui::PointerButton::Primary) {
            let next = clamp_pan(
                self.pan + response.drag_delta(),
                natural_size * self.zoom,
                viewport.size(),
            );
            if (next - self.pan).length_sq() > f32::EPSILON {
                self.pan = next;
                self.mode = ZoomMode::Manual;
            }
        }
        if response.hovered() {
            let (pointer, zoom_delta, scroll_delta) = ui.input(|input| {
                (
                    input.pointer.latest_pos(),
                    input.zoom_delta(),
                    input.smooth_scroll_delta(),
                )
            });
            if let Some(pointer) = pointer
                && (zoom_delta - 1.0).abs() > f32::EPSILON
            {
                let next = (self.zoom * zoom_delta).clamp(fit, MAX_ZOOM);
                self.pan = zoom_pan_anchored(self.pan, self.zoom, next, pointer, viewport.center());
                self.zoom = next;
                if next <= fit + f32::EPSILON {
                    self.mode = ZoomMode::Fit;
                    self.pan = Vec2::ZERO;
                } else {
                    self.mode = ZoomMode::Manual;
                }
            } else if scroll_delta != Vec2::ZERO {
                let next = clamp_pan(
                    self.pan + scroll_delta,
                    natural_size * self.zoom,
                    viewport.size(),
                );
                if (next - self.pan).length_sq() > f32::EPSILON {
                    self.pan = next;
                    self.mode = ZoomMode::Manual;
                }
            }
        }
        if response.double_clicked() {
            // Toggle between fit and 100% regardless of how large fit is —
            // the old `zoom < 0.9` test dead-ended at fit for small pages.
            let at_fit = self.mode == ZoomMode::Fit || (self.zoom - fit).abs() <= 0.01;
            self.pending_command = Some(if at_fit {
                ViewCommand::OneToOne
            } else {
                ViewCommand::Fit
            });
            ui.ctx().request_repaint();
        }

        let display_size = natural_size * self.zoom;
        self.pan = clamp_pan(self.pan, display_size, viewport.size());
        let page_rect = image_rect(viewport, natural_size, self.zoom, self.pan);
        if let Some((visible, uv)) = clipped_image(page_rect, viewport) {
            painter.image(texture.id(), visible, uv, Color32::WHITE);
            painter.rect_stroke(
                page_rect,
                0.0,
                Stroke::new(1.0, Color32::from_gray(140)),
                egui::StrokeKind::Inside,
            );
        }
        if self.texture.is_none() && self.load_error.is_none() {
            painter.text(
                Pos2::new(viewport.center().x, viewport.top() + 18.0),
                egui::Align2::CENTER_CENTER,
                "Wczytywanie pełnej rozdzielczości…",
                egui::FontId::proportional(13.0),
                Color32::from_gray(210),
            );
        }
        ui.ctx().set_cursor_icon(if response.dragged() {
            CursorIcon::Grabbing
        } else if response.hovered() {
            CursorIcon::Grab
        } else {
            CursorIcon::Default
        });
        response.on_hover_text(
            "Ctrl + kółko: powiększ · kółko: przesuń · przeciągnij: przesuń · podwójne kliknięcie: 100% / dopasuj",
        );
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            if ui.button("−").on_hover_text("Pomniejsz").clicked() {
                self.pending_command = Some(ViewCommand::ZoomBy(1.0 / ZOOM_STEP));
            }
            if ui
                .button("100%")
                .on_hover_text("Jeden piksel obrazu na piksel ekranu")
                .clicked()
            {
                self.pending_command = Some(ViewCommand::OneToOne);
            }
            if ui.button("+").on_hover_text("Powiększ").clicked() {
                self.pending_command = Some(ViewCommand::ZoomBy(ZOOM_STEP));
            }
            if ui.button("Dopasuj").clicked() {
                self.pending_command = Some(ViewCommand::Fit);
            }
            let zoom_percent = (self.zoom * 100.0).round();
            ui.separator();
            // Real density on the A4 canvas, not a hardcoded claim: the short
            // page edge maps to 210 mm.
            let dpi = (self.source_px.x.min(self.source_px.y) / (210.0 / 25.4)).round();
            let density = if dpi > 0.0 {
                format!(" · {dpi:.0} DPI")
            } else {
                String::new()
            };
            ui.label(
                egui::RichText::new(format!(
                    "{} × {} px{density} · {zoom_percent:.0}%",
                    self.source_px.x as u32, self.source_px.y as u32
                ))
                .small()
                .color(Color32::from_gray(95)),
            );
        });
    }
}

fn decode_full_texture(
    context: &egui::Context,
    key: PageTextureKey,
    page: &EncodedPage,
    max_texture_side: usize,
) -> Result<(TextureHandle, Vec2), String> {
    let image = crate::document::decode_page(page)
        .map_err(|error| format!("Nie można otworzyć pełnego podglądu: {error}"))?;
    let image = if key.quarter_turns.is_multiple_of(4) {
        image
    } else {
        crate::document::rotate_rgb(&image, key.quarter_turns)
    };
    // Zoom and the pixel readout stay in source coordinates even when the
    // uploaded texture had to be reduced to the GPU limit.
    let source_px = Vec2::new(image.width() as f32, image.height() as f32);
    let scaled = fit_texture_image(&image, max_texture_side);
    let texture_source = scaled.as_ref().unwrap_or(&image);
    let color_image = ColorImage::from_rgb(
        [
            texture_source.width() as usize,
            texture_source.height() as usize,
        ],
        texture_source.as_raw(),
    );
    Ok((
        context.load_texture(
            format!("review-full-{}-{}-t{}", key.id, key.revision, key.quarter_turns),
            color_image,
            TextureOptions::LINEAR,
        ),
        source_px,
    ))
}

/// Returns a copy reduced to the GPU texture limit, or `None` when the image
/// already fits. Uploading an oversized texture asserts inside the painter.
pub fn fit_texture_image(
    image: &image::RgbImage,
    max_texture_side: usize,
) -> Option<image::RgbImage> {
    let max_side = max_texture_side.min(u32::MAX as usize).max(64) as u32;
    let longest = image.width().max(image.height());
    if longest <= max_side {
        return None;
    }
    let scale = max_side as f32 / longest as f32;
    let width = ((image.width() as f32 * scale).floor() as u32).clamp(1, max_side);
    let height = ((image.height() as f32 * scale).floor() as u32).clamp(1, max_side);
    Some(image::imageops::resize(
        image,
        width,
        height,
        image::imageops::FilterType::CatmullRom,
    ))
}

fn natural_size_points(source_px: Vec2, pixels_per_point: f32) -> Vec2 {
    source_px / pixels_per_point.max(0.01)
}

fn fit_zoom(source_px: Vec2, viewport: Vec2, pixels_per_point: f32) -> f32 {
    let natural = natural_size_points(source_px, pixels_per_point);
    if natural.x <= 0.0 || natural.y <= 0.0 || viewport.x <= 0.0 || viewport.y <= 0.0 {
        return 1.0;
    }
    (viewport.x / natural.x)
        .min(viewport.y / natural.y)
        .clamp(MIN_ZOOM, 1.0)
}

fn image_rect(viewport: Rect, natural: Vec2, zoom: f32, pan: Vec2) -> Rect {
    Rect::from_center_size(viewport.center() + pan, natural * zoom)
}

fn zoom_pan_anchored(pan: Vec2, old: f32, new: f32, anchor: Pos2, center: Pos2) -> Vec2 {
    if old <= 0.0 {
        return pan;
    }
    let delta = anchor - center;
    delta - (delta - pan) * (new / old)
}

fn clamp_pan(pan: Vec2, display: Vec2, viewport: Vec2) -> Vec2 {
    let overflow_x = ((display.x - viewport.x) * 0.5).max(0.0);
    let overflow_y = ((display.y - viewport.y) * 0.5).max(0.0);
    Vec2::new(
        pan.x.clamp(-overflow_x, overflow_x),
        pan.y.clamp(-overflow_y, overflow_y),
    )
}

fn clipped_image(image: Rect, viewport: Rect) -> Option<(Rect, Rect)> {
    if image.width() <= 0.0 || image.height() <= 0.0 {
        return None;
    }
    let visible = image.intersect(viewport);
    if !visible.is_positive() {
        return None;
    }
    let uv = Rect::from_min_max(
        Pos2::new(
            (visible.min.x - image.min.x) / image.width(),
            (visible.min.y - image.min.y) / image.height(),
        ),
        Pos2::new(
            (visible.max.x - image.min.x) / image.width(),
            (visible.max.y - image.min.y) / image.height(),
        ),
    );
    Some((visible, uv))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_texture_image_only_shrinks_oversized_images() {
        let image = image::RgbImage::new(4160, 3120);
        let scaled = fit_texture_image(&image, 4096).expect("must shrink");
        assert_eq!(scaled.width(), 4096);
        assert!(scaled.height() <= 4096 && scaled.height() > 0);
        assert!(fit_texture_image(&image, 4160).is_none());
        assert!(fit_texture_image(&image, 8192).is_none());
    }

    #[test]
    fn fit_zoom_uses_limiting_axis_and_never_upscales() {
        assert!(
            (fit_zoom(Vec2::new(1000.0, 2000.0), Vec2::new(500.0, 500.0), 1.0) - 0.25).abs()
                < 0.001
        );
        assert_eq!(
            fit_zoom(Vec2::new(100.0, 100.0), Vec2::new(500.0, 500.0), 1.0),
            1.0
        );
    }

    #[test]
    fn natural_size_honors_display_scale() {
        assert_eq!(
            natural_size_points(Vec2::new(2480.0, 3508.0), 1.25),
            Vec2::new(1984.0, 2806.4)
        );
    }

    #[test]
    fn anchored_zoom_keeps_pointer_source_position_stable() {
        let center = Pos2::new(500.0, 400.0);
        let anchor = Pos2::new(700.0, 500.0);
        let old_pan = Vec2::new(20.0, -10.0);
        let new_pan = zoom_pan_anchored(old_pan, 0.5, 1.0, anchor, center);
        let before = (anchor - center - old_pan) / 0.5;
        let after = (anchor - center - new_pan) / 1.0;
        assert!((before - after).length() < 0.001);
    }

    #[test]
    fn clamp_pan_centers_small_axis_and_limits_large_axis() {
        assert_eq!(
            clamp_pan(
                Vec2::new(100.0, -500.0),
                Vec2::new(300.0, 1200.0),
                Vec2::new(500.0, 600.0),
            ),
            Vec2::new(0.0, -300.0)
        );
    }

    #[test]
    fn clipping_produces_matching_uv_coordinates() {
        let image = Rect::from_min_size(Pos2::new(-50.0, -25.0), Vec2::new(200.0, 100.0));
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0));
        let (visible, uv) = clipped_image(image, viewport).expect("visible intersection");
        assert_eq!(
            visible,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(100.0, 75.0))
        );
        assert_eq!(uv.min, Pos2::new(0.25, 0.25));
        assert_eq!(uv.max, Pos2::new(0.75, 1.0));
    }

    #[test]
    fn clipping_rejects_non_overlapping_image() {
        let image = Rect::from_min_size(Pos2::new(200.0, 200.0), Vec2::splat(50.0));
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0));
        assert!(clipped_image(image, viewport).is_none());
    }
}
