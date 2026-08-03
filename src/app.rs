use crate::camera::{CameraController, CameraEvent};
use crate::document::{
    CropPoint, ScannedPage, detect_document_corners, process_page, rotate_page_clockwise, save_pdf,
};
use crate::storage::{
    FolderInfo, PdfInfo, Settings, create_folder, default_library_root, ensure_library,
    list_folders, list_pdfs, load_settings, rename_folder, save_settings, unique_pdf_path,
};
use eframe::egui::{
    self, Align, Button, Color32, ColorImage, CornerRadius, FontId, Frame, Id, Layout, Margin,
    Pos2, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, UiBuilder, Vec2,
};
use image::RgbImage;
use std::path::PathBuf;
use std::time::Duration;

const BLUE: Color32 = Color32::from_rgb(38, 101, 180);
const BLUE_DARK: Color32 = Color32::from_rgb(24, 72, 130);
const PALE_BLUE: Color32 = Color32::from_rgb(231, 241, 252);
const BACKGROUND: Color32 = Color32::from_rgb(246, 248, 251);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Library,
    Folder,
    Scan,
    Crop,
    Review,
}

pub struct DocumentScannerApp {
    screen: Screen,
    settings: Settings,
    library_root: PathBuf,
    folders: Vec<FolderInfo>,
    selected_folder: Option<FolderInfo>,
    pdfs: Vec<PdfInfo>,

    camera: Option<CameraController>,
    camera_status: String,
    camera_ready: bool,
    preview_texture: Option<TextureHandle>,
    preview_size: [usize; 2],

    crop_image: Option<RgbImage>,
    crop_texture: Option<TextureHandle>,
    crop_points: [CropPoint; 4],
    crop_busy: bool,

    pages: Vec<ScannedPage>,
    page_textures: Vec<TextureHandle>,
    selected_page: usize,
    filename: String,
    saved_path: Option<PathBuf>,

    show_new_folder: bool,
    new_folder_name: String,
    show_rename_folder: bool,
    rename_folder_name: String,
    show_settings: bool,
    show_save: bool,
    show_saved: bool,
    show_delete_confirm: bool,
    show_exit_confirm: bool,
    allow_exit: bool,
    message: Option<String>,
}

impl DocumentScannerApp {
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        configure_style(&context.egui_ctx);
        let settings = load_settings();
        let library_root = settings
            .library_root
            .clone()
            .unwrap_or_else(default_library_root);
        let mut app = Self {
            screen: Screen::Library,
            settings,
            library_root,
            folders: Vec::new(),
            selected_folder: None,
            pdfs: Vec::new(),
            camera: None,
            camera_status: String::new(),
            camera_ready: false,
            preview_texture: None,
            preview_size: [0, 0],
            crop_image: None,
            crop_texture: None,
            crop_points: [
                CropPoint::new(0.06, 0.06),
                CropPoint::new(0.94, 0.06),
                CropPoint::new(0.94, 0.94),
                CropPoint::new(0.06, 0.94),
            ],
            crop_busy: false,
            pages: Vec::new(),
            page_textures: Vec::new(),
            selected_page: 0,
            filename: String::new(),
            saved_path: None,
            show_new_folder: false,
            new_folder_name: String::new(),
            show_rename_folder: false,
            rename_folder_name: String::new(),
            show_settings: false,
            show_save: false,
            show_saved: false,
            show_delete_confirm: false,
            show_exit_confirm: false,
            allow_exit: false,
            message: None,
        };
        if let Err(error) = ensure_library(&app.library_root) {
            app.message = Some(error);
        }
        app.refresh_folders();
        app.restore_last_folder();
        app
    }

    fn restore_last_folder(&mut self) {
        let Some(last_name) = self.settings.last_folder.clone() else {
            return;
        };
        let Some(folder) = self
            .folders
            .iter()
            .find(|folder| folder.name == last_name)
            .cloned()
        else {
            return;
        };
        self.open_folder(folder);
    }

    fn refresh_folders(&mut self) {
        match list_folders(&self.library_root) {
            Ok(folders) => self.folders = folders,
            Err(error) => self.message = Some(error),
        }
    }

    fn refresh_pdfs(&mut self) {
        let Some(folder) = &self.selected_folder else {
            self.pdfs.clear();
            return;
        };
        match list_pdfs(&folder.path) {
            Ok(pdfs) => self.pdfs = pdfs,
            Err(error) => self.message = Some(error),
        }
    }

    fn open_folder(&mut self, folder: FolderInfo) {
        self.settings.last_folder = Some(folder.name.clone());
        self.selected_folder = Some(folder);
        self.screen = Screen::Folder;
        self.refresh_pdfs();
        let _ = save_settings(&self.settings);
    }

    fn back_to_library(&mut self) {
        self.selected_folder = None;
        self.settings.last_folder = None;
        self.screen = Screen::Library;
        self.refresh_folders();
        let _ = save_settings(&self.settings);
    }

    fn begin_scan(&mut self) {
        self.pages.clear();
        self.page_textures.clear();
        self.selected_page = 0;
        self.filename.clear();
        self.saved_path = None;
        self.start_camera();
    }

    fn start_camera(&mut self) {
        self.stop_camera();
        self.camera_status = "Łączenie z IRIScan Visualizer 7…".to_owned();
        self.camera_ready = false;
        self.preview_texture = None;
        self.camera = Some(CameraController::start());
        self.screen = Screen::Scan;
    }

    fn stop_camera(&mut self) {
        if let Some(mut camera) = self.camera.take() {
            camera.stop();
        }
    }

    fn poll_camera(&mut self, context: &egui::Context) {
        let mut events = Vec::new();
        if let Some(camera) = &self.camera {
            while let Some(event) = camera.try_event() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                CameraEvent::Ready {
                    device_name,
                    width,
                    height,
                } => {
                    self.camera_ready = true;
                    self.camera_status = format!("{device_name} · {width} × {height} px");
                }
                CameraEvent::Preview(image) => self.update_preview_texture(context, &image),
                CameraEvent::Error(error) => {
                    self.camera_ready = false;
                    self.preview_texture = None;
                    self.camera_status = error;
                }
            }
        }
        if self.screen == Screen::Scan {
            context.request_repaint_after(Duration::from_millis(35));
        }
    }

    fn update_preview_texture(&mut self, context: &egui::Context, image: &RgbImage) {
        let color_image = rgb_to_color_image(image);
        self.preview_size = color_image.size;
        if let Some(texture) = &mut self.preview_texture {
            texture.set(color_image, TextureOptions::LINEAR);
        } else {
            self.preview_texture =
                Some(context.load_texture("podglad-kamery", color_image, TextureOptions::LINEAR));
        }
    }

    fn capture(&mut self, context: &egui::Context) {
        let Some(image) = self
            .camera
            .as_ref()
            .and_then(CameraController::latest_full_image)
        else {
            self.message = Some("Poczekaj, aż pojawi się obraz z kamery.".to_owned());
            return;
        };
        self.stop_camera();
        let image = image.as_ref().clone();
        self.crop_points = detect_document_corners(&image);
        self.crop_texture = Some(context.load_texture(
            "kadrowanie",
            rgb_to_color_image(&image),
            TextureOptions::LINEAR,
        ));
        self.crop_image = Some(image);
        self.screen = Screen::Crop;
    }

    fn rotate_crop_source(&mut self, context: &egui::Context) {
        let Some(image) = self.crop_image.take() else {
            return;
        };
        let rotated = image::imageops::rotate90(&image);
        self.crop_points = detect_document_corners(&rotated);
        self.crop_texture = Some(context.load_texture(
            "kadrowanie-obrocone",
            rgb_to_color_image(&rotated),
            TextureOptions::LINEAR,
        ));
        self.crop_image = Some(rotated);
    }

    fn accept_crop(&mut self, context: &egui::Context) {
        let Some(image) = &self.crop_image else {
            return;
        };
        self.crop_busy = true;
        match process_page(image, self.crop_points) {
            Ok(page) => {
                let texture = context.load_texture(
                    format!("strona-{}", self.pages.len()),
                    rgb_to_color_image(&page.review_image),
                    TextureOptions::LINEAR,
                );
                self.pages.push(page);
                self.page_textures.push(texture);
                self.selected_page = self.pages.len() - 1;
                self.crop_image = None;
                self.crop_texture = None;
                self.screen = Screen::Review;
            }
            Err(error) => self.message = Some(error),
        }
        self.crop_busy = false;
    }

    fn rotate_selected_page(&mut self, context: &egui::Context) {
        let Some(page) = self.pages.get(self.selected_page).cloned() else {
            return;
        };
        match rotate_page_clockwise(&page) {
            Ok(rotated) => {
                self.page_textures[self.selected_page] = context.load_texture(
                    format!("strona-{}-obrot", self.selected_page),
                    rgb_to_color_image(&rotated.review_image),
                    TextureOptions::LINEAR,
                );
                self.pages[self.selected_page] = rotated;
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn delete_selected_page(&mut self) {
        if self.pages.is_empty() {
            return;
        }
        self.pages.remove(self.selected_page);
        let _ = self.page_textures.remove(self.selected_page);
        if self.pages.is_empty() {
            self.start_camera();
        } else {
            self.selected_page = self.selected_page.min(self.pages.len() - 1);
        }
    }

    fn move_selected_page(&mut self, direction: isize) {
        if self.pages.is_empty() {
            return;
        }
        let target = self.selected_page as isize + direction;
        if !(0..self.pages.len() as isize).contains(&target) {
            return;
        }
        let target = target as usize;
        self.pages.swap(self.selected_page, target);
        self.page_textures.swap(self.selected_page, target);
        self.selected_page = target;
    }

    fn save_current_document(&mut self) {
        let Some(folder) = &self.selected_folder else {
            self.message = Some("Najpierw wybierz folder docelowy.".to_owned());
            return;
        };
        let path = match unique_pdf_path(&folder.path, &self.filename) {
            Ok(path) => path,
            Err(error) => {
                self.message = Some(error);
                return;
            }
        };
        match save_pdf(&path, &self.pages) {
            Ok(()) => {
                self.saved_path = Some(path);
                self.show_save = false;
                self.show_saved = true;
                self.refresh_pdfs();
                self.refresh_folders();
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn cancel_scan(&mut self) {
        self.stop_camera();
        self.crop_image = None;
        self.crop_texture = None;
        self.pages.clear();
        self.page_textures.clear();
        self.screen = Screen::Folder;
        self.refresh_pdfs();
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        Frame::new()
            .fill(Color32::WHITE)
            .inner_margin(Margin::symmetric(24, 14))
            .show(ui, |ui| {
                two_sided(
                    ui,
                    42.0,
                    |ui| {
                        ui.label(
                            RichText::new("Skaner dokumentów")
                                .size(25.0)
                                .strong()
                                .color(BLUE_DARK),
                        );
                    },
                    |ui| {
                        if ui.button("Ustawienia").clicked() {
                            self.show_settings = true;
                        }
                        if let Some(folder) = &self.selected_folder {
                            ui.label(
                                RichText::new(&folder.name)
                                    .size(16.0)
                                    .color(Color32::DARK_GRAY),
                            );
                        }
                    },
                );
            });
    }

    fn library_ui(&mut self, ui: &mut egui::Ui) {
        page_container(ui, |ui| {
            two_sided(
                ui,
                58.0,
                |ui| {
                    ui.vertical(|ui| {
                        ui.heading("Foldery dokumentów");
                        ui.label("Wybierz folder lub utwórz nowy.");
                    });
                },
                |ui| {
                    if primary_button(ui, "Nowy folder").clicked() {
                        self.new_folder_name.clear();
                        self.show_new_folder = true;
                    }
                },
            );
            ui.add_space(22.0);
            if self.folders.is_empty() {
                empty_card(
                    ui,
                    "Nie ma jeszcze żadnych folderów.",
                    "Kliknij „Nowy folder”, aby rozpocząć.",
                );
                return;
            }
            let tile_width = 235.0;
            let columns = ((ui.available_width() / (tile_width + 14.0)).floor() as usize).max(1);
            let mut clicked_folder = None;
            egui::Grid::new("foldery")
                .num_columns(columns)
                .spacing([14.0, 14.0])
                .show(ui, |ui| {
                    for (index, folder) in self.folders.iter().enumerate() {
                        let label = format!("{}\n\n{} plików PDF", folder.name, folder.pdf_count);
                        if ui
                            .add_sized(
                                [tile_width, 125.0],
                                Button::new(RichText::new(label).size(17.0))
                                    .fill(Color32::WHITE)
                                    .stroke(Stroke::new(1.0, Color32::from_gray(215)))
                                    .corner_radius(12.0),
                            )
                            .clicked()
                        {
                            clicked_folder = Some(folder.clone());
                        }
                        if (index + 1) % columns == 0 {
                            ui.end_row();
                        }
                    }
                });
            if let Some(folder) = clicked_folder {
                self.open_folder(folder);
            }
        });
    }

    fn folder_ui(&mut self, ui: &mut egui::Ui) {
        page_container(ui, |ui| {
            let folder_name = self
                .selected_folder
                .as_ref()
                .map(|folder| folder.name.clone())
                .unwrap_or_default();
            let folder_path = self
                .selected_folder
                .as_ref()
                .map(|folder| folder.path.clone());
            let (go_back, (new_scan, rename, open_in_explorer)) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut go_back = false;
                    ui.horizontal(|ui| {
                        go_back = ui.button("Wróć do folderów").clicked();
                        ui.add_space(8.0);
                        ui.heading(&folder_name);
                    });
                    go_back
                },
                |ui| {
                    let new_scan = primary_button(ui, "Nowy skan").clicked();
                    let rename = ui.button("Zmień nazwę folderu").clicked();
                    let open_in_explorer = ui.button("Otwórz w Eksploratorze").clicked();
                    (new_scan, rename, open_in_explorer)
                },
            );
            if go_back {
                self.back_to_library();
                return;
            }
            if new_scan {
                self.begin_scan();
                return;
            }
            if rename {
                self.rename_folder_name = folder_name;
                self.show_rename_folder = true;
            }
            if open_in_explorer
                && let Some(path) = folder_path
                && let Err(error) = open::that_detached(path)
            {
                self.message = Some(format!("Nie można otworzyć folderu: {error}"));
            }
            ui.add_space(20.0);
            if self.pdfs.is_empty() {
                empty_card(
                    ui,
                    "Ten folder jest pusty.",
                    "Kliknij „Nowy skan”, aby dodać pierwszy dokument.",
                );
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for pdf in &self.pdfs {
                        Frame::new()
                            .fill(Color32::WHITE)
                            .corner_radius(10.0)
                            .stroke(Stroke::new(1.0, Color32::from_gray(222)))
                            .inner_margin(Margin::symmetric(18, 14))
                            .show(ui, |ui| {
                                let (_, open_pdf) = two_sided(
                                    ui,
                                    38.0,
                                    |ui| {
                                        ui.label(RichText::new(&pdf.name).size(16.0));
                                    },
                                    |ui| ui.button("Otwórz PDF").clicked(),
                                );
                                if open_pdf && let Err(error) = open::that_detached(&pdf.path) {
                                    self.message = Some(format!("Nie można otworzyć PDF: {error}"));
                                }
                            });
                        ui.add_space(8.0);
                    }
                });
            }
        });
    }

    fn scan_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        self.poll_camera(context);
        page_container(ui, |ui| {
            let page_number = self.pages.len() + 1;
            let camera_ready = self.camera_ready;
            let camera_status = self.camera_status.clone();
            let (cancel, ()) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut cancel = false;
                    ui.horizontal(|ui| {
                        cancel = ui.button("Anuluj skanowanie").clicked();
                        ui.add_space(10.0);
                        ui.heading(format!("Skanowanie · strona {page_number}"));
                    });
                    cancel
                },
                |ui| {
                    let color = if camera_ready {
                        Color32::DARK_GREEN
                    } else {
                        Color32::DARK_GRAY
                    };
                    ui.label(RichText::new(camera_status).color(color));
                },
            );
            if cancel {
                self.cancel_scan();
                return;
            }
            ui.add_space(14.0);
            let controls_min = ui.available_rect_before_wrap().min;
            let controls_width = (ui.clip_rect().right() - controls_min.x).max(0.0);
            let (controls_rect, _) =
                ui.allocate_exact_size(Vec2::new(controls_width, 54.0), Sense::hover());
            let mut controls_ui = ui.new_child(
                UiBuilder::new()
                    .id_salt("scan-controls")
                    .max_rect(controls_rect)
                    .layout(Layout::left_to_right(Align::Center).with_main_align(Align::Center)),
            );
            let capture = controls_ui.add_enabled(
                self.camera_ready && self.preview_texture.is_some(),
                Button::new(
                    RichText::new("Zrób zdjęcie")
                        .size(20.0)
                        .strong()
                        .color(Color32::WHITE),
                )
                .fill(BLUE)
                .corner_radius(12.0)
                .min_size(Vec2::new(240.0, 54.0)),
            );
            if capture.clicked() {
                self.capture(context);
            }
            if !self.camera_ready && controls_ui.button("Spróbuj ponownie").clicked() {
                self.start_camera();
            }
            ui.add_space(12.0);
            let preview_min = ui.available_rect_before_wrap().min;
            let preview_max = Pos2::new(ui.clip_rect().right(), ui.clip_rect().bottom());
            if preview_max.x > preview_min.x && preview_max.y > preview_min.y {
                let preview_rect = Rect::from_min_max(preview_min, preview_max);
                ui.allocate_rect(preview_rect, Sense::hover());
                let painter = ui.painter_at(preview_rect);
                painter.rect_filled(preview_rect, 12.0, Color32::from_gray(30));
                if let Some(texture) = &self.preview_texture {
                    let image_bounds = preview_rect.shrink(12.0);
                    let size = fit_size(
                        Vec2::new(self.preview_size[0] as f32, self.preview_size[1] as f32),
                        image_bounds.size(),
                    );
                    painter.image(
                        texture.id(),
                        Rect::from_center_size(image_bounds.center(), size),
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    painter.text(
                        preview_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        &self.camera_status,
                        FontId::proportional(18.0),
                        Color32::WHITE,
                    );
                }
            }
        });
    }

    fn crop_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        page_container(ui, |ui| {
            two_sided(
                ui,
                42.0,
                |ui| {
                    ui.heading("Dopasuj obszar dokumentu");
                },
                |ui| {
                    ui.label("Przeciągnij niebieskie punkty do narożników kartki.");
                },
            );
            ui.add_space(12.0);
            let available = Vec2::new(
                ui.available_width(),
                (ui.available_height() - 100.0).max(320.0),
            );
            let (response, painter) = ui.allocate_painter(available, Sense::hover());
            painter.rect_filled(response.rect, 12.0, Color32::from_gray(28));
            if let (Some(texture), Some(image)) = (&self.crop_texture, &self.crop_image) {
                let image_size = fit_size(
                    Vec2::new(image.width() as f32, image.height() as f32),
                    available - Vec2::splat(12.0),
                );
                let image_rect = Rect::from_center_size(response.rect.center(), image_size);
                painter.image(
                    texture.id(),
                    image_rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
                let positions = self.crop_points.map(|point| {
                    Pos2::new(
                        image_rect.left() + point.x * image_rect.width(),
                        image_rect.top() + point.y * image_rect.height(),
                    )
                });
                for edge in 0..4 {
                    painter.line_segment(
                        [positions[edge], positions[(edge + 1) % 4]],
                        Stroke::new(3.0, Color32::from_rgb(70, 165, 255)),
                    );
                }
                for (index, position) in positions.iter().enumerate() {
                    let handle_rect = Rect::from_center_size(*position, Vec2::splat(44.0));
                    let drag =
                        ui.interact(handle_rect, Id::new(("crop-handle", index)), Sense::drag());
                    if drag.dragged() {
                        let pointer = drag.interact_pointer_pos().unwrap_or(*position);
                        let normalized = CropPoint::new(
                            ((pointer.x - image_rect.left()) / image_rect.width()).clamp(0.0, 1.0),
                            ((pointer.y - image_rect.top()) / image_rect.height()).clamp(0.0, 1.0),
                        );
                        self.crop_points[index] = constrain_corner(index, normalized);
                    }
                    painter.circle_filled(*position, 11.0, Color32::WHITE);
                    painter.circle_filled(*position, 7.0, BLUE);
                }
            }
            ui.add_space(12.0);
            let ((retake, rotate, redetect), accept) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut retake = false;
                    let mut rotate = false;
                    let mut redetect = false;
                    ui.horizontal(|ui| {
                        retake = ui.button("Powtórz zdjęcie").clicked();
                        rotate = ui.button("Obróć").clicked();
                        redetect = ui.button("Wykryj ponownie").clicked();
                    });
                    (retake, rotate, redetect)
                },
                |ui| {
                    ui.add_enabled(
                        !self.crop_busy,
                        Button::new(
                            RichText::new("Użyj tej strony")
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(BLUE)
                        .corner_radius(10.0)
                        .min_size(Vec2::new(180.0, 44.0)),
                    )
                    .clicked()
                },
            );
            if retake {
                self.crop_image = None;
                self.crop_texture = None;
                self.start_camera();
                return;
            }
            if rotate {
                self.rotate_crop_source(context);
            }
            if redetect && let Some(image) = &self.crop_image {
                self.crop_points = detect_document_corners(image);
            }
            if accept {
                self.accept_crop(context);
            }
        });
    }

    fn review_ui(&mut self, ui: &mut egui::Ui, context: &egui::Context) {
        page_container(ui, |ui| {
            let page_count = self.pages.len();
            let page_count_text = polish_page_count(page_count);
            let (cancel, (finish, add_page)) = two_sided(
                ui,
                48.0,
                |ui| {
                    let mut cancel = false;
                    ui.horizontal(|ui| {
                        cancel = ui.button("Anuluj dokument").clicked();
                        ui.add_space(10.0);
                        ui.heading(format!("Dokument · {page_count_text}"));
                    });
                    cancel
                },
                |ui| {
                    let finish = primary_button(ui, "Zakończ skanowanie").clicked();
                    let add_page = ui.button("Dodaj stronę").clicked();
                    (finish, add_page)
                },
            );
            if cancel {
                self.cancel_scan();
                return;
            }
            if finish {
                self.show_save = true;
            }
            if add_page {
                self.start_camera();
                return;
            }
            ui.add_space(15.0);
            let content_min = ui.available_rect_before_wrap().min;
            let content_max = Pos2::new(ui.clip_rect().right(), ui.clip_rect().bottom());
            if content_max.x <= content_min.x || content_max.y <= content_min.y {
                return;
            }
            let content_rect = Rect::from_min_max(content_min, content_max);
            ui.allocate_rect(content_rect, Sense::hover());

            let sidebar_width = 210.0_f32.min(content_rect.width() * 0.3);
            let gap = 14.0;
            let sidebar_rect = Rect::from_min_size(
                content_rect.min,
                Vec2::new(sidebar_width, content_rect.height()),
            );
            let main_rect = Rect::from_min_max(
                Pos2::new(sidebar_rect.right() + gap, content_rect.top()),
                content_rect.max,
            );

            let sidebar_painter = ui.painter_at(sidebar_rect);
            sidebar_painter.rect_filled(sidebar_rect, 12.0, Color32::WHITE);
            sidebar_painter.rect_stroke(
                sidebar_rect,
                12.0,
                Stroke::new(1.0, Color32::from_gray(218)),
                egui::StrokeKind::Inside,
            );
            let sidebar_inner = sidebar_rect.shrink(12.0);
            let mut sidebar_ui = ui.new_child(
                UiBuilder::new()
                    .id_salt("review-pages")
                    .max_rect(sidebar_inner)
                    .layout(Layout::top_down(Align::Min)),
            );
            sidebar_ui.heading(format!("Strony ({page_count})"));
            sidebar_ui.add_space(6.0);
            let mut clicked_page = None;
            egui::ScrollArea::vertical()
                .max_height((sidebar_inner.height() - 44.0).max(0.0))
                .show(&mut sidebar_ui, |ui| {
                    for (index, texture) in self.page_textures.iter().enumerate() {
                        let selected = index == self.selected_page;
                        Frame::new()
                            .fill(if selected { PALE_BLUE } else { Color32::WHITE })
                            .stroke(Stroke::new(
                                if selected { 2.0 } else { 1.0 },
                                if selected {
                                    BLUE
                                } else {
                                    Color32::from_gray(220)
                                },
                            ))
                            .corner_radius(8.0)
                            .inner_margin(8)
                            .show(ui, |ui| {
                                ui.vertical_centered(|ui| {
                                    let image_size = fit_size(
                                        texture.size_vec2(),
                                        Vec2::new((sidebar_inner.width() - 32.0).max(40.0), 150.0),
                                    );
                                    if ui
                                        .add(
                                            egui::Image::new(texture)
                                                .fit_to_exact_size(image_size)
                                                .sense(Sense::click()),
                                        )
                                        .clicked()
                                    {
                                        clicked_page = Some(index);
                                    }
                                    if ui
                                        .selectable_label(selected, format!("Strona {}", index + 1))
                                        .clicked()
                                    {
                                        clicked_page = Some(index);
                                    }
                                });
                            });
                        ui.add_space(8.0);
                    }
                });
            if let Some(index) = clicked_page {
                self.selected_page = index;
            }

            let controls_height = 58.0;
            let preview_rect = Rect::from_min_max(
                main_rect.min,
                Pos2::new(
                    main_rect.right(),
                    main_rect.bottom() - controls_height - 10.0,
                ),
            );
            let preview_painter = ui.painter_at(preview_rect);
            preview_painter.rect_filled(preview_rect, 12.0, Color32::from_gray(235));
            if let Some(texture) = self.page_textures.get(self.selected_page) {
                let bounds = preview_rect.shrink(14.0);
                let image_size = fit_size(texture.size_vec2(), bounds.size());
                let image_rect = Rect::from_center_size(bounds.center(), image_size);
                preview_painter.image(
                    texture.id(),
                    image_rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
                preview_painter.rect_stroke(
                    image_rect,
                    0.0,
                    Stroke::new(1.0, Color32::from_gray(150)),
                    egui::StrokeKind::Inside,
                );
            }

            let controls_rect = Rect::from_min_max(
                Pos2::new(main_rect.left(), preview_rect.bottom() + 10.0),
                main_rect.max,
            );
            let mut controls_ui = ui.new_child(
                UiBuilder::new()
                    .id_salt("review-controls")
                    .max_rect(controls_rect)
                    .layout(Layout::left_to_right(Align::Center).with_main_align(Align::Center)),
            );
            let can_move_left = self.selected_page > 0;
            let can_move_right = self.selected_page + 1 < self.pages.len();
            if controls_ui
                .add_enabled(can_move_left, Button::new("← W lewo"))
                .clicked()
            {
                self.move_selected_page(-1);
            }
            if controls_ui
                .add_enabled(can_move_right, Button::new("W prawo →"))
                .clicked()
            {
                self.move_selected_page(1);
            }
            if controls_ui.button("Obróć").clicked() {
                self.rotate_selected_page(context);
            }
            if controls_ui
                .button(RichText::new("Usuń stronę").color(Color32::DARK_RED))
                .clicked()
            {
                self.show_delete_confirm = true;
            }
        });
    }

    fn dialogs(&mut self, context: &egui::Context) {
        if self.show_new_folder {
            egui::Window::new("Nowy folder")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label("Nazwa folderu:");
                    let response = ui.add_sized(
                        [360.0, 34.0],
                        egui::TextEdit::singleline(&mut self.new_folder_name),
                    );
                    response.request_focus();
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Anuluj").clicked() {
                            self.show_new_folder = false;
                        }
                        if primary_button(ui, "Utwórz folder").clicked() {
                            match create_folder(&self.library_root, &self.new_folder_name) {
                                Ok(folder) => {
                                    self.show_new_folder = false;
                                    self.refresh_folders();
                                    self.open_folder(folder);
                                }
                                Err(error) => self.message = Some(error),
                            }
                        }
                    });
                });
        }

        if self.show_rename_folder {
            egui::Window::new("Zmień nazwę folderu")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label("Nowa nazwa:");
                    ui.add_sized(
                        [360.0, 34.0],
                        egui::TextEdit::singleline(&mut self.rename_folder_name),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Anuluj").clicked() {
                            self.show_rename_folder = false;
                        }
                        if primary_button(ui, "Zapisz nazwę").clicked()
                            && let Some(folder) = self.selected_folder.clone()
                        {
                            match rename_folder(&folder, &self.rename_folder_name) {
                                Ok(folder) => {
                                    self.selected_folder = Some(folder.clone());
                                    self.settings.last_folder = Some(folder.name.clone());
                                    let _ = save_settings(&self.settings);
                                    self.show_rename_folder = false;
                                    self.refresh_folders();
                                    self.refresh_pdfs();
                                }
                                Err(error) => self.message = Some(error),
                            }
                        }
                    });
                });
        }

        if self.show_settings {
            egui::Window::new("Ustawienia")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label(RichText::new("Folder biblioteki").strong());
                    ui.label(self.library_root.display().to_string());
                    ui.add_space(10.0);
                    if ui.button("Zmień lokalizację").clicked()
                        && let Some(path) = rfd::FileDialog::new().pick_folder()
                    {
                        if let Err(error) = ensure_library(&path) {
                            self.message = Some(error);
                        } else {
                            self.library_root = path.clone();
                            self.settings.library_root = Some(path);
                            self.settings.last_folder = None;
                            self.selected_folder = None;
                            self.screen = Screen::Library;
                            if let Err(error) = save_settings(&self.settings) {
                                self.message = Some(error);
                            }
                            self.refresh_folders();
                        }
                    }
                    ui.add_space(12.0);
                    if primary_button(ui, "Gotowe").clicked() {
                        self.show_settings = false;
                    }
                });
        }

        if self.show_save {
            egui::Window::new("Zapisz dokument")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label(format!("Liczba stron: {}", self.pages.len()));
                    ui.label("Nazwa pliku:");
                    let response = ui.add_sized(
                        [390.0, 36.0],
                        egui::TextEdit::singleline(&mut self.filename)
                            .hint_text("np. Umowa - Kowalski"),
                    );
                    response.request_focus();
                    ui.label(
                        RichText::new("Rozszerzenie .pdf zostanie dodane automatycznie.")
                            .small()
                            .color(Color32::GRAY),
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("Wróć").clicked() {
                            self.show_save = false;
                        }
                        if primary_button(ui, "Zapisz PDF").clicked() {
                            self.save_current_document();
                        }
                    });
                });
        }

        if self.show_saved {
            egui::Window::new("Dokument zapisany")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label(
                        RichText::new("PDF został zapisany pomyślnie.")
                            .size(18.0)
                            .color(Color32::DARK_GREEN),
                    );
                    if let Some(path) = &self.saved_path {
                        ui.label(path.display().to_string());
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Otwórz PDF").clicked()
                            && let Some(path) = &self.saved_path
                            && let Err(error) = open::that_detached(path)
                        {
                            self.message = Some(format!("Nie można otworzyć PDF: {error}"));
                        }
                        if primary_button(ui, "Gotowe").clicked() {
                            self.show_saved = false;
                            self.pages.clear();
                            self.page_textures.clear();
                            self.screen = Screen::Folder;
                            self.refresh_pdfs();
                        }
                    });
                });
        }

        if self.show_delete_confirm {
            egui::Window::new("Usunąć stronę?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    if self.pages.len() == 1 {
                        ui.label("To jest jedyna strona dokumentu.");
                        ui.label("Po usunięciu program wróci do skanowania.");
                    } else {
                        ui.label(format!("Usunąć stronę {}?", self.selected_page + 1));
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Anuluj").clicked() {
                            self.show_delete_confirm = false;
                        }
                        if ui
                            .button(RichText::new("Usuń stronę").color(Color32::DARK_RED))
                            .clicked()
                        {
                            self.show_delete_confirm = false;
                            self.delete_selected_page();
                        }
                    });
                });
        }

        if self.show_exit_confirm {
            egui::Window::new("Niezapisany dokument")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.label("Zeskanowane strony nie zostały jeszcze zapisane.");
                    ui.label("Czy na pewno chcesz zamknąć program?");
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.button("Wróć do dokumentu").clicked() {
                            self.show_exit_confirm = false;
                        }
                        if ui
                            .button(
                                RichText::new("Zamknij bez zapisywania").color(Color32::DARK_RED),
                            )
                            .clicked()
                        {
                            self.allow_exit = true;
                            self.show_exit_confirm = false;
                            context.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });
        }

        if let Some(message) = self.message.clone() {
            egui::Window::new("Informacja")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(context, |ui| {
                    ui.set_max_width(480.0);
                    ui.label(message);
                    ui.add_space(10.0);
                    if primary_button(ui, "OK").clicked() {
                        self.message = None;
                    }
                });
        }
    }
}

impl eframe::App for DocumentScannerApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        let close_requested = context.input(|input| input.viewport().close_requested());
        let has_unsaved_scan =
            self.saved_path.is_none() && (self.crop_image.is_some() || !self.pages.is_empty());
        if close_requested && has_unsaved_scan && !self.allow_exit {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_exit_confirm = true;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        Frame::new().fill(BACKGROUND).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            self.top_bar(ui);
            match self.screen {
                Screen::Library => self.library_ui(ui),
                Screen::Folder => self.folder_ui(ui),
                Screen::Scan => self.scan_ui(ui, &context),
                Screen::Crop => self.crop_ui(ui, &context),
                Screen::Review => self.review_ui(ui, &context),
            }
        });
        self.dialogs(&context);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_camera();
        let _ = save_settings(&self.settings);
    }
}

fn configure_style(context: &egui::Context) {
    context.set_theme(egui::Theme::Light);
    let mut style = (*context.style_of(egui::Theme::Light)).clone();
    style.spacing.button_padding = Vec2::new(16.0, 10.0);
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.visuals = egui::Visuals::light();
    style.visuals.widgets.active.bg_fill = BLUE;
    style.visuals.widgets.hovered.bg_fill = PALE_BLUE;
    style.visuals.window_corner_radius = CornerRadius::same(12);
    style
        .text_styles
        .insert(egui::TextStyle::Heading, FontId::proportional(24.0));
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(16.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(16.0));
    context.set_style_of(egui::Theme::Light, style);
}

fn page_container(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .inner_margin(Margin::symmetric(28, 22))
        .show(ui, |ui| add_contents(ui));
}

fn two_sided<Left, Right>(
    ui: &mut egui::Ui,
    height: f32,
    add_left: impl FnOnce(&mut egui::Ui) -> Left,
    add_right: impl FnOnce(&mut egui::Ui) -> Right,
) -> (Left, Right) {
    let min = ui.available_rect_before_wrap().min;
    let width = (ui.clip_rect().right() - min.x).max(0.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let id = ui.next_auto_id();
    let mut left_ui = ui.new_child(
        UiBuilder::new()
            .id_salt((id, "left"))
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    let mut right_ui = ui.new_child(
        UiBuilder::new()
            .id_salt((id, "right"))
            .max_rect(rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    let left = add_left(&mut left_ui);
    let right = add_right(&mut right_ui);
    (left, right)
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        Button::new(RichText::new(text).strong().color(Color32::WHITE))
            .fill(BLUE)
            .corner_radius(9.0),
    )
}

fn empty_card(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, Color32::from_gray(222)))
        .corner_radius(12.0)
        .inner_margin(36)
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(20.0).strong());
                ui.label(RichText::new(subtitle).color(Color32::GRAY));
            });
        });
}

fn rgb_to_color_image(image: &RgbImage) -> ColorImage {
    ColorImage::from_rgb(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}

fn fit_size(source: Vec2, bounds: Vec2) -> Vec2 {
    if source.x <= 0.0 || source.y <= 0.0 {
        return Vec2::ZERO;
    }
    let scale = (bounds.x / source.x).min(bounds.y / source.y).max(0.0);
    source * scale
}

fn constrain_corner(index: usize, point: CropPoint) -> CropPoint {
    match index {
        0 => CropPoint::new(point.x.min(0.49), point.y.min(0.49)),
        1 => CropPoint::new(point.x.max(0.51), point.y.min(0.49)),
        2 => CropPoint::new(point.x.max(0.51), point.y.max(0.51)),
        3 => CropPoint::new(point.x.min(0.49), point.y.max(0.51)),
        _ => point,
    }
}

fn polish_page_count(count: usize) -> String {
    let word = if count == 1 {
        "strona"
    } else if (2..=4).contains(&(count % 10)) && !(12..=14).contains(&(count % 100)) {
        "strony"
    } else {
        "stron"
    };
    format!("{count} {word}")
}
