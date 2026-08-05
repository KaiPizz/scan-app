use crate::library::{LibrarySnapshot, SortMode, normalize_search_text, search_and_sort};
use crate::storage::PdfInfo;
use eframe::egui::{
    self, Align, Align2, Button, Color32, CornerRadius, FontId, Frame, Id, Layout, Margin,
    RichText, Sense, Stroke, UiBuilder, Vec2,
};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SIDEBAR_WIDTH: f32 = 240.0;
const DETAILS_WIDTH: f32 = 286.0;
const DETAILS_BREAKPOINT: f32 = 1_350.0;
const ROW_HEIGHT: f32 = 62.0;
const CONTROL_HEIGHT: f32 = 44.0;
const MUTED_TEXT: Color32 = Color32::from_rgb(65, 69, 77);
const BLUE: Color32 = Color32::from_rgb(38, 101, 180);
const PALE_BLUE: Color32 = Color32::from_rgb(231, 241, 252);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LibrarySection {
    #[default]
    Home,
    All,
    Recent,
}

#[derive(Debug)]
pub struct LibraryViewState {
    pub section: LibrarySection,
    pub query: String,
    pub sort: SortMode,
    selected: BTreeSet<PathBuf>,
    anchor: Option<PathBuf>,
    search_focus_requested: bool,
    selection_revision: u64,
    visible_cache_key: Option<VisibleCacheKey>,
    visible_documents: Vec<PdfInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisibleCacheKey {
    snapshot_revision: u64,
    query: String,
    sort: SortMode,
    section: LibrarySection,
    folder_scope: Option<PathBuf>,
    recent_documents: Vec<PathBuf>,
}

impl Default for LibraryViewState {
    fn default() -> Self {
        Self {
            section: LibrarySection::Home,
            query: String::new(),
            sort: SortMode::Newest,
            selected: BTreeSet::new(),
            anchor: None,
            search_focus_requested: false,
            selection_revision: 0,
            visible_cache_key: None,
            visible_documents: Vec::new(),
        }
    }
}

impl LibraryViewState {
    pub fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected.iter().cloned().collect()
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    pub fn select_only(&mut self, path: PathBuf) {
        self.selected.clear();
        self.selected.insert(path.clone());
        self.anchor = Some(path);
    }

    pub fn replace_path(&mut self, source: &Path, destination: &Path) {
        if self.selected.remove(source) {
            self.selected.insert(destination.to_path_buf());
        }
        if self.anchor.as_deref() == Some(source) {
            self.anchor = Some(destination.to_path_buf());
        }
    }

    pub fn remove_paths<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        for path in paths {
            self.selected.remove(path);
            if self.anchor.as_ref() == Some(path) {
                self.anchor = None;
            }
        }
    }

    pub fn replace_prefix(&mut self, source: &Path, destination: &Path) {
        let replacements: Vec<(PathBuf, PathBuf)> = self
            .selected
            .iter()
            .filter_map(|path| {
                path.strip_prefix(source)
                    .ok()
                    .map(|suffix| (path.clone(), destination.join(suffix)))
            })
            .collect();
        for (source_path, destination_path) in replacements {
            self.replace_path(&source_path, &destination_path);
        }
    }
}

#[derive(Debug, Clone)]
pub enum LibraryAction {
    ChangeSection(LibrarySection),
    SelectFolder(PathBuf),
    RenameFolder(PathBuf),
    NewFolder,
    NewScan(Option<PathBuf>),
    Refresh,
    Open(PathBuf),
    Edit(PathBuf),
    Rename(PathBuf),
    Move(Vec<PathBuf>),
    Recycle(Vec<PathBuf>),
    OpenFolderInExplorer(PathBuf),
    OpenSettings,
}

#[derive(Clone, Copy)]
pub struct LibraryViewInput<'a> {
    pub snapshot: &'a LibrarySnapshot,
    pub snapshot_revision: u64,
    pub recent_documents: &'a [PathBuf],
    pub selected_folder: Option<&'a Path>,
    pub preferred_scan_folder: Option<&'a Path>,
    pub library_root: &'a Path,
    pub loading: bool,
    pub keyboard_enabled: bool,
}

pub fn show_library_view(
    ui: &mut egui::Ui,
    state: &mut LibraryViewState,
    input: LibraryViewInput<'_>,
) -> Vec<LibraryAction> {
    if state.selection_revision != input.snapshot_revision {
        let valid_paths: HashSet<&Path> = input
            .snapshot
            .pdfs
            .iter()
            .map(|pdf| pdf.path.as_path())
            .collect();
        state
            .selected
            .retain(|path| valid_paths.contains(path.as_path()));
        if state
            .anchor
            .as_ref()
            .is_some_and(|path| !state.selected.contains(path))
        {
            state.anchor = None;
        }
        state.selection_revision = input.snapshot_revision;
    }

    refresh_visible_cache(state, input);
    let documents = std::mem::take(&mut state.visible_documents);
    handle_keyboard(ui, state, input.keyboard_enabled, &documents);

    let mut actions = keyboard_actions(ui, state, input.keyboard_enabled);
    let available = ui.available_size();
    let show_details = available.x >= DETAILS_BREAKPOINT;
    let details_width = if show_details { DETAILS_WIDTH } else { 0.0 };
    let center_width = (available.x - SIDEBAR_WIDTH - details_width - 24.0).max(360.0);

    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            Vec2::new(SIDEBAR_WIDTH, available.y),
            Layout::top_down(Align::Min),
            |ui| show_sidebar(ui, state, input, &mut actions),
        );
        ui.separator();
        ui.allocate_ui_with_layout(
            Vec2::new(center_width, available.y),
            Layout::top_down(Align::Min),
            |ui| show_document_list(ui, state, input, &documents, &mut actions),
        );
        if show_details {
            ui.separator();
            ui.allocate_ui_with_layout(
                Vec2::new(DETAILS_WIDTH, available.y),
                Layout::top_down(Align::Min),
                |ui| show_details_panel(ui, state, input, &mut actions),
            );
        }
    });

    state.visible_documents = documents;
    actions
}

fn show_sidebar(
    ui: &mut egui::Ui,
    state: &mut LibraryViewState,
    input: LibraryViewInput<'_>,
    actions: &mut Vec<LibraryAction>,
) {
    ui.set_width(SIDEBAR_WIDTH);
    ui.spacing_mut().item_spacing.y = 4.0;
    let compact_height = ui.available_height() < 430.0;
    let row_height = if compact_height { 40.0 } else { CONTROL_HEIGHT };
    if !compact_height {
        ui.label(RichText::new("Biblioteka").size(22.0).strong());
        ui.add_space(10.0);
    }

    if section_button(
        ui,
        "Strona główna",
        None,
        state.section == LibrarySection::Home && input.selected_folder.is_none(),
        row_height,
    )
    .clicked()
    {
        state.section = LibrarySection::Home;
        state.query.clear();
        state.clear_selection();
        actions.push(LibraryAction::ChangeSection(LibrarySection::Home));
    }
    if section_button(
        ui,
        "Ostatnie",
        None,
        state.section == LibrarySection::Recent && input.selected_folder.is_none(),
        row_height,
    )
    .clicked()
    {
        state.section = LibrarySection::Recent;
        state.query.clear();
        state.clear_selection();
        actions.push(LibraryAction::ChangeSection(LibrarySection::Recent));
    }
    if section_button(
        ui,
        "Wszystkie dokumenty",
        Some(input.snapshot.pdfs.len()),
        state.section == LibrarySection::All && input.selected_folder.is_none(),
        row_height,
    )
    .clicked()
    {
        state.section = LibrarySection::All;
        state.query.clear();
        state.clear_selection();
        actions.push(LibraryAction::ChangeSection(LibrarySection::All));
    }

    ui.add_space(if compact_height { 8.0 } else { 16.0 });
    egui::containers::Sides::new()
        .height(row_height)
        .shrink_left()
        .show(
            ui,
            |ui| {
                ui.label(RichText::new("Foldery").size(15.0).strong());
            },
            |ui| {
                ui.spacing_mut().interact_size.y = row_height;
                if ui
                    .add_sized([112.0, row_height], Button::new("+ Nowy"))
                    .on_hover_text("Utwórz nowy folder")
                    .clicked()
                {
                    actions.push(LibraryAction::NewFolder);
                }
            },
        );

    let remaining = ui.available_rect_before_wrap();
    let location_height: f32 = if compact_height { 92.0 } else { 126.0 };
    let location_height = location_height.min(remaining.height());
    let location_top = (remaining.bottom() - location_height).max(remaining.top());
    let folders_bottom = (location_top - 10.0).max(remaining.top());
    let folders_rect =
        egui::Rect::from_min_max(remaining.min, egui::pos2(remaining.right(), folders_bottom));
    let location_rect =
        egui::Rect::from_min_max(egui::pos2(remaining.left(), location_top), remaining.max);

    if folders_rect.height() > 1.0 {
        let mut folders_ui = ui.new_child(
            UiBuilder::new()
                .id_salt("library-folder-area")
                .max_rect(folders_rect)
                .layout(Layout::top_down(Align::Min)),
        );
        folders_ui.set_clip_rect(folders_rect);
        egui::ScrollArea::vertical()
            .id_salt("library-folders")
            .auto_shrink([false, false])
            .show(&mut folders_ui, |ui| {
                let normalized_query = normalize_search_text(&state.query);
                let query_tokens: Vec<&str> = normalized_query.split_whitespace().collect();
                let mut shown = 0;
                for folder in &input.snapshot.folders {
                    let normalized_name = normalize_search_text(&folder.name);
                    if !query_tokens
                        .iter()
                        .all(|token| normalized_name.contains(token))
                    {
                        continue;
                    }
                    shown += 1;
                    let selected = input.selected_folder == Some(folder.path.as_path());
                    if sidebar_row(
                        ui,
                        &folder.name,
                        Some(folder.pdf_count),
                        selected,
                        row_height,
                    )
                    .clicked()
                    {
                        state.section = LibrarySection::All;
                        state.query.clear();
                        state.clear_selection();
                        actions.push(LibraryAction::SelectFolder(folder.path.clone()));
                    }
                }
                if shown == 0 && !state.query.trim().is_empty() {
                    ui.label(
                        RichText::new("Brak pasujących folderów")
                            .size(14.0)
                            .color(MUTED_TEXT),
                    );
                }
            });
    }

    let mut location_ui = ui.new_child(
        UiBuilder::new()
            .id_salt("library-location-area")
            .max_rect(location_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    location_ui.set_clip_rect(location_rect);
    show_location_card(
        &mut location_ui,
        input.library_root,
        compact_height,
        actions,
    );
    ui.advance_cursor_after_rect(remaining);
}

fn section_button(
    ui: &mut egui::Ui,
    text: &str,
    count: Option<usize>,
    selected: bool,
    height: f32,
) -> egui::Response {
    sidebar_row(ui, text, count, selected, height)
}

fn sidebar_row(
    ui: &mut egui::Ui,
    text: &str,
    count: Option<usize>,
    selected: bool,
    height: f32,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let fill = if selected {
        PALE_BLUE
    } else if response.hovered() {
        Color32::from_rgb(239, 244, 249)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(8), fill);

    let max_chars = if count.is_some() { 21 } else { 27 };
    let visible_text = ellipsize(text, max_chars);
    let text_color = if selected {
        BLUE
    } else {
        Color32::from_gray(42)
    };
    let painter = ui.painter().with_clip_rect(rect.shrink(8.0));
    painter.text(
        rect.left_center() + Vec2::new(12.0, 0.0),
        Align2::LEFT_CENTER,
        visible_text,
        FontId::proportional(15.0),
        text_color,
    );
    if let Some(count) = count {
        painter.text(
            rect.right_center() - Vec2::new(12.0, 0.0),
            Align2::RIGHT_CENTER,
            count.to_string(),
            FontId::proportional(14.0),
            MUTED_TEXT,
        );
    }
    if text.chars().count() > max_chars {
        response.on_hover_text(text)
    } else {
        response
    }
}

fn show_location_card(
    ui: &mut egui::Ui,
    library_root: &Path,
    compact: bool,
    actions: &mut Vec<LibraryAction>,
) {
    let full_path = library_root.display().to_string();
    let location_name = library_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| full_path.clone());
    Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, Color32::from_gray(214)))
        .corner_radius(10.0)
        .inner_margin(if compact { 8 } else { 12 })
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = if compact { 3.0 } else { 4.0 };
            ui.spacing_mut().interact_size.y = if compact { 40.0 } else { CONTROL_HEIGHT };
            ui.set_width((SIDEBAR_WIDTH - if compact { 16.0 } else { 24.0 }).max(1.0));
            if compact {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("Lokalizacja  ·  {full_path}"))
                            .size(14.0)
                            .strong()
                            .color(MUTED_TEXT),
                    )
                    .truncate(),
                )
                .on_hover_text(&full_path);
                ui.add_space(4.0);
            } else {
                ui.label(
                    RichText::new("Lokalizacja biblioteki")
                        .size(14.0)
                        .strong()
                        .color(MUTED_TEXT),
                );
                ui.add(
                    egui::Label::new(
                        RichText::new(if location_name == full_path {
                            full_path.clone()
                        } else {
                            format!("{location_name}  ·  {full_path}")
                        })
                        .size(15.0)
                        .color(MUTED_TEXT),
                    )
                    .truncate(),
                )
                .on_hover_text(&full_path);
                ui.add_space(6.0);
            }
            let button_height = if compact { 40.0 } else { CONTROL_HEIGHT };
            ui.horizontal(|ui| {
                if ui
                    .add_sized([86.0, button_height], Button::new("Otwórz"))
                    .on_hover_text("Otwórz folder biblioteki w Eksploratorze")
                    .clicked()
                {
                    actions.push(LibraryAction::OpenFolderInExplorer(
                        library_root.to_path_buf(),
                    ));
                }
                if ui
                    .add_sized([86.0, button_height], Button::new("Zmień"))
                    .on_hover_text("Wybierz inną lokalizację biblioteki")
                    .clicked()
                {
                    actions.push(LibraryAction::OpenSettings);
                }
            });
        });
}

fn ellipsize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut shortened: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    shortened.push('…');
    shortened
}

fn show_document_list(
    ui: &mut egui::Ui,
    state: &mut LibraryViewState,
    input: LibraryViewInput<'_>,
    documents: &[PdfInfo],
    actions: &mut Vec<LibraryAction>,
) {
    let title = if !state.query.trim().is_empty() {
        format!("Wyniki wyszukiwania ({})", documents.len())
    } else if let Some(folder) = input.selected_folder {
        if folder == input.library_root {
            "Główna lokalizacja".to_owned()
        } else {
            folder
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Folder".to_owned())
        }
    } else {
        match state.section {
            LibrarySection::Home => "Ostatnio dodane".to_owned(),
            LibrarySection::All => "Wszystkie dokumenty".to_owned(),
            LibrarySection::Recent => "Ostatnio otwierane".to_owned(),
        }
    };

    egui::containers::Sides::new()
        .height(CONTROL_HEIGHT)
        .shrink_left()
        .truncate()
        .show(
            ui,
            |ui| {
                ui.add(egui::Label::new(RichText::new(title).size(22.0).strong()).truncate());
            },
            |ui| {
                let destination = scan_destination(input);
                let has_destination = destination.is_some();
                let scan_button = ui.add_enabled(
                    has_destination,
                    Button::new(RichText::new("Nowy skan").color(Color32::WHITE).strong())
                        .fill(BLUE)
                        .min_size(Vec2::new(112.0, CONTROL_HEIGHT)),
                );
                if scan_button.clicked() {
                    actions.push(LibraryAction::NewScan(destination));
                }
                if !has_destination {
                    scan_button.on_disabled_hover_text("Najpierw wybierz albo utwórz folder.");
                }
                if ui
                    .add(Button::new("Odśwież").min_size(Vec2::new(88.0, CONTROL_HEIGHT)))
                    .clicked()
                {
                    actions.push(LibraryAction::Refresh);
                }
            },
        );
    if let Some(folder) = input.selected_folder {
        ui.horizontal_wrapped(|ui| {
            if folder != input.library_root
                && ui
                    .add(
                        Button::new("Zmień nazwę folderu").min_size(Vec2::new(0.0, CONTROL_HEIGHT)),
                    )
                    .clicked()
            {
                actions.push(LibraryAction::RenameFolder(folder.to_path_buf()));
            }
            if ui
                .add(Button::new("Otwórz folder").min_size(Vec2::new(0.0, CONTROL_HEIGHT)))
                .clicked()
            {
                actions.push(LibraryAction::OpenFolderInExplorer(folder.to_path_buf()));
            }
        });
    }

    let show_sort = !state.query.trim().is_empty()
        || input.selected_folder.is_some()
        || state.section == LibrarySection::All;
    let narrow_toolbar = show_sort && ui.available_width() < 520.0;
    let search = if narrow_toolbar {
        let response = ui.add_sized(
            [ui.available_width(), CONTROL_HEIGHT],
            egui::TextEdit::singleline(&mut state.query)
                .id(Id::new("library-search"))
                .hint_text("Szukaj po nazwie pliku lub folderu…"),
        );
        show_sort_picker(ui, state);
        response
    } else {
        ui.horizontal(|ui| {
            let search_width = if show_sort {
                (ui.available_width() - 176.0).max(180.0)
            } else {
                ui.available_width()
            };
            let search = ui.add_sized(
                [search_width, CONTROL_HEIGHT],
                egui::TextEdit::singleline(&mut state.query)
                    .id(Id::new("library-search"))
                    .hint_text("Szukaj po nazwie pliku lub folderu…"),
            );
            if show_sort {
                show_sort_picker(ui, state);
            }
            search
        })
        .inner
    };
    if state.search_focus_requested {
        search.request_focus();
        state.search_focus_requested = false;
    }

    if !state.selected.is_empty() {
        selected_action_bar(ui, state, input, documents, actions);
    } else {
        ui.add_space(2.0);
    }

    if input.loading && input.snapshot.pdfs.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(50.0);
            ui.spinner();
            ui.label("Wczytywanie biblioteki…");
        });
        return;
    }
    if documents.is_empty() {
        let (title, detail) = if state.query.trim().is_empty()
            && state.section == LibrarySection::Recent
            && input.selected_folder.is_none()
        {
            (
                "Brak ostatnio otwieranych dokumentów",
                "Tutaj pojawią się pliki otwarte z biblioteki.",
            )
        } else if state.query.trim().is_empty() {
            (
                "Brak dokumentów",
                "Zeskanuj dokument albo wybierz inny folder.",
            )
        } else {
            (
                "Brak wyników",
                "Zmień wyszukiwaną frazę lub wyczyść pole wyszukiwania.",
            )
        };
        empty_state(ui, title, detail);
        return;
    }

    let columns = TableColumns::new(
        (ui.available_width() - 38.0).max(320.0),
        input.selected_folder.is_none(),
        ui.spacing().item_spacing.x,
    );
    Frame::new()
        .fill(Color32::from_rgb(238, 242, 247))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                checkbox_cell(ui, |ui| {
                    let all_selected = documents
                        .iter()
                        .all(|pdf| state.selected.contains(&pdf.path));
                    let mut checked = all_selected;
                    let response = ui
                        .checkbox(&mut checked, "")
                        .on_hover_text(if all_selected {
                            "Odznacz wszystkie widoczne dokumenty"
                        } else {
                            "Zaznacz wszystkie widoczne dokumenty"
                        });
                    if response.changed() {
                        set_visible_selection(state, documents, checked);
                    }
                });
                table_label(ui, columns.name, RichText::new("Nazwa").strong());
                if columns.show_folder {
                    table_label(ui, columns.folder, RichText::new("Folder").strong());
                }
                table_label(
                    ui,
                    columns.modified,
                    RichText::new("Zmodyfikowano").strong(),
                );
                if columns.show_size {
                    table_label(ui, columns.size, RichText::new("Rozmiar").strong());
                }
            });
        });

    let list_height = ui.available_height().max(80.0);
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), list_height),
        Layout::top_down(Align::Min),
        |ui| {
            egui::ScrollArea::vertical()
                .id_salt("library-documents")
                .auto_shrink([false, false])
                .show_rows(ui, ROW_HEIGHT, documents.len(), |ui, rows| {
                    for index in rows {
                        document_row(ui, state, documents, index, columns, actions);
                    }
                });
        },
    );
}

fn set_visible_selection(state: &mut LibraryViewState, documents: &[PdfInfo], selected: bool) {
    if selected {
        for pdf in documents {
            state.selected.insert(pdf.path.clone());
        }
        state.anchor = documents.first().map(|pdf| pdf.path.clone());
    } else {
        for pdf in documents {
            state.selected.remove(&pdf.path);
        }
        if state
            .anchor
            .as_ref()
            .is_some_and(|path| documents.iter().any(|pdf| &pdf.path == path))
        {
            state.anchor = None;
        }
    }
}

#[derive(Clone, Copy)]
struct TableColumns {
    name: f32,
    folder: f32,
    modified: f32,
    size: f32,
    show_folder: bool,
    show_size: bool,
    compact_folder: bool,
}

impl TableColumns {
    fn new(width: f32, may_show_folder: bool, spacing: f32) -> Self {
        let show_folder = may_show_folder && width >= 700.0;
        let show_size = width >= 650.0;
        let folder = 138.0;
        let modified = 118.0;
        let size = 82.0;
        let visible_columns = 3 + usize::from(show_folder) + usize::from(show_size);
        let total_spacing = spacing * visible_columns.saturating_sub(1) as f32;
        let fixed = 32.0
            + modified
            + if show_folder { folder } else { 0.0 }
            + if show_size { size } else { 0.0 };
        let name = (width - fixed - total_spacing).max(150.0);
        Self {
            name,
            folder,
            modified,
            size,
            show_folder,
            show_size,
            compact_folder: may_show_folder && !show_folder,
        }
    }
}

fn checkbox_cell(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    ui.allocate_ui_with_layout(
        Vec2::new(32.0, CONTROL_HEIGHT),
        Layout::centered_and_justified(egui::Direction::LeftToRight),
        add_contents,
    );
}

fn table_label(ui: &mut egui::Ui, width: f32, text: RichText) -> egui::Response {
    ui.allocate_ui_with_layout(
        Vec2::new(width, CONTROL_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| ui.add(egui::Label::new(text).truncate()),
    )
    .inner
}

fn selected_action_bar(
    ui: &mut egui::Ui,
    state: &mut LibraryViewState,
    input: LibraryViewInput<'_>,
    documents: &[PdfInfo],
    actions: &mut Vec<LibraryAction>,
) {
    let paths = state.selected_paths();
    Frame::new()
        .fill(Color32::from_rgb(231, 241, 252))
        .stroke(Stroke::new(1.0, Color32::from_rgb(176, 207, 239)))
        .corner_radius(8.0)
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            let visible_paths: HashSet<&Path> =
                documents.iter().map(|pdf| pdf.path.as_path()).collect();
            let visible_count = paths
                .iter()
                .filter(|path| visible_paths.contains(path.as_path()))
                .count();
            let hidden_count = paths.len().saturating_sub(visible_count);
            let selected_label = if hidden_count == 0 {
                format!("Wybrano: {}", paths.len())
            } else {
                format!("Wybrano: {} (poza widokiem: {hidden_count})", paths.len())
            };
            ui.label(RichText::new(selected_label).strong());
            ui.horizontal_wrapped(|ui| {
                if paths.len() == 1 {
                    let path = paths[0].clone();
                    if ui.button("Otwórz").clicked() {
                        actions.push(LibraryAction::Open(path.clone()));
                    }
                    if ui.button("Edytuj").clicked() {
                        actions.push(LibraryAction::Edit(path.clone()));
                    }
                    if ui.button("Zmień nazwę").clicked() {
                        actions.push(LibraryAction::Rename(path));
                    }
                }
                if ui.button("Przenieś do…").clicked() {
                    actions.push(LibraryAction::Move(paths.clone()));
                }
                if ui
                    .button(RichText::new("Do Kosza").color(Color32::DARK_RED))
                    .clicked()
                {
                    actions.push(LibraryAction::Recycle(paths.clone()));
                }
                if ui.button("Wyczyść zaznaczenie").clicked() {
                    state.clear_selection();
                }
                if paths.len() == 1
                    && let Some(pdf) = find_pdf(input.snapshot, &paths[0])
                    && ui.button("Folder w Eksploratorze").clicked()
                {
                    actions.push(LibraryAction::OpenFolderInExplorer(pdf.folder_path.clone()));
                }
            });
        });
}

fn document_row(
    ui: &mut egui::Ui,
    state: &mut LibraryViewState,
    documents: &[PdfInfo],
    index: usize,
    columns: TableColumns,
    actions: &mut Vec<LibraryAction>,
) {
    let pdf = &documents[index];
    let selected = state.selected.contains(&pdf.path);
    let fill = if selected {
        Color32::from_rgb(231, 241, 252)
    } else if index.is_multiple_of(2) {
        Color32::WHITE
    } else {
        Color32::from_rgb(250, 251, 253)
    };
    let row = Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, Color32::from_gray(228)))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.scope_builder(UiBuilder::new().sense(Sense::click()), |ui| {
                ui.set_height(ROW_HEIGHT - 14.0);
                ui.horizontal(|ui| {
                    checkbox_cell(ui, |ui| {
                        let mut checked = selected;
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                state.selected.insert(pdf.path.clone());
                                state.anchor = Some(pdf.path.clone());
                            } else {
                                state.selected.remove(&pdf.path);
                            }
                        }
                    });
                    if columns.compact_folder {
                        ui.allocate_ui_with_layout(
                            Vec2::new(columns.name, CONTROL_HEIGHT),
                            Layout::top_down(Align::Min).with_main_align(Align::Center),
                            |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                ui.add(
                                    egui::Label::new(RichText::new(&pdf.name).strong()).truncate(),
                                )
                                .on_hover_text(&pdf.name);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&pdf.folder_name)
                                            .size(13.0)
                                            .color(MUTED_TEXT),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&pdf.folder_name);
                            },
                        );
                    } else {
                        table_label(ui, columns.name, RichText::new(&pdf.name).strong())
                            .on_hover_text(&pdf.name);
                    }
                    if columns.show_folder {
                        table_label(ui, columns.folder, RichText::new(&pdf.folder_name))
                            .on_hover_text(&pdf.folder_name);
                    }
                    table_label(
                        ui,
                        columns.modified,
                        RichText::new(format_relative_time(pdf.modified_secs)),
                    );
                    if columns.show_size {
                        table_label(ui, columns.size, RichText::new(format_size(pdf.size_bytes)));
                    }
                });
            })
            .response
        });
    let response = row.inner;
    if response.double_clicked() {
        state.select_only(pdf.path.clone());
        actions.push(LibraryAction::Open(pdf.path.clone()));
    } else if response.clicked() {
        let modifiers = ui.input(|input| input.modifiers);
        if modifiers.shift {
            select_range(state, documents, index);
        } else if modifiers.command || modifiers.ctrl {
            if !state.selected.remove(&pdf.path) {
                state.selected.insert(pdf.path.clone());
            }
            state.anchor = Some(pdf.path.clone());
        } else {
            state.select_only(pdf.path.clone());
        }
    }
}

fn select_range(state: &mut LibraryViewState, documents: &[PdfInfo], clicked: usize) {
    let anchor = state
        .anchor
        .as_ref()
        .and_then(|path| documents.iter().position(|pdf| &pdf.path == path))
        .unwrap_or(clicked);
    let (start, end) = if anchor <= clicked {
        (anchor, clicked)
    } else {
        (clicked, anchor)
    };
    state.selected.clear();
    for pdf in &documents[start..=end] {
        state.selected.insert(pdf.path.clone());
    }
    state.anchor = Some(documents[anchor].path.clone());
}

fn show_details_panel(
    ui: &mut egui::Ui,
    state: &LibraryViewState,
    input: LibraryViewInput<'_>,
    actions: &mut Vec<LibraryAction>,
) {
    ui.set_width(DETAILS_WIDTH);
    ui.label(RichText::new("Szczegóły").size(20.0).strong());
    ui.add_space(12.0);
    let Some(path) = state.selected.iter().next() else {
        ui.label(RichText::new("Wybierz dokument, aby zobaczyć szczegóły.").color(MUTED_TEXT));
        return;
    };
    if state.selected.len() > 1 {
        ui.label(RichText::new(format!("Wybrano {} dokumentów.", state.selected.len())).strong());
        ui.label("Użyj paska działań nad listą, aby przenieść je albo wysłać do Kosza.");
        return;
    }
    let Some(pdf) = find_pdf(input.snapshot, path) else {
        return;
    };
    Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, Color32::from_gray(220)))
        .corner_radius(10.0)
        .inner_margin(16)
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("PDF")
                        .size(24.0)
                        .strong()
                        .color(Color32::from_rgb(38, 101, 180)),
                );
            });
            ui.add_space(10.0);
            ui.label(RichText::new(&pdf.name).strong());
            ui.label(format!("Folder: {}", pdf.folder_name));
            ui.label(format!("Rozmiar: {}", format_size(pdf.size_bytes)));
            ui.label(format!(
                "Zmodyfikowano: {}",
                format_relative_time(pdf.modified_secs)
            ));
            ui.add_space(8.0);
            let full_path = pdf.path.display().to_string();
            ui.add(
                egui::Label::new(RichText::new(&full_path).size(14.0).color(MUTED_TEXT)).truncate(),
            )
            .on_hover_text(full_path);
            ui.add_space(12.0);
            if ui
                .add_sized(
                    [ui.available_width(), CONTROL_HEIGHT],
                    Button::new("Otwórz PDF"),
                )
                .clicked()
            {
                actions.push(LibraryAction::Open(pdf.path.clone()));
            }
            if ui
                .add_sized(
                    [ui.available_width(), CONTROL_HEIGHT],
                    Button::new("Edytuj skan"),
                )
                .clicked()
            {
                actions.push(LibraryAction::Edit(pdf.path.clone()));
            }
            if ui
                .add_sized(
                    [ui.available_width(), CONTROL_HEIGHT],
                    Button::new("Zmień nazwę"),
                )
                .clicked()
            {
                actions.push(LibraryAction::Rename(pdf.path.clone()));
            }
            if ui
                .add_sized(
                    [ui.available_width(), CONTROL_HEIGHT],
                    Button::new("Przenieś do…"),
                )
                .clicked()
            {
                actions.push(LibraryAction::Move(vec![pdf.path.clone()]));
            }
            if ui
                .add_sized(
                    [ui.available_width(), CONTROL_HEIGHT],
                    Button::new("Otwórz folder"),
                )
                .clicked()
            {
                actions.push(LibraryAction::OpenFolderInExplorer(pdf.folder_path.clone()));
            }
            if ui
                .add_sized(
                    [ui.available_width(), CONTROL_HEIGHT],
                    Button::new(RichText::new("Przenieś do Kosza").color(Color32::DARK_RED)),
                )
                .clicked()
            {
                actions.push(LibraryAction::Recycle(vec![pdf.path.clone()]));
            }
        });
}

fn refresh_visible_cache(state: &mut LibraryViewState, input: LibraryViewInput<'_>) {
    let key = VisibleCacheKey {
        snapshot_revision: input.snapshot_revision,
        query: state.query.clone(),
        sort: state.sort,
        section: state.section,
        folder_scope: input.selected_folder.map(Path::to_path_buf),
        recent_documents: input.recent_documents.to_vec(),
    };
    if state.visible_cache_key.as_ref() == Some(&key) {
        return;
    }
    state.visible_documents = visible_documents_for_key(input.snapshot, &key);
    state.visible_cache_key = Some(key);
}

fn visible_documents_for_key(snapshot: &LibrarySnapshot, key: &VisibleCacheKey) -> Vec<PdfInfo> {
    if !key.query.trim().is_empty() {
        // Inside a folder the search stays scoped to that folder — a global
        // result list would show identically named files from other folders
        // while the folder column is hidden.
        return search_and_sort(snapshot, &key.query, key.sort, key.folder_scope.as_deref());
    }
    if let Some(folder) = key.folder_scope.as_deref() {
        return search_and_sort(snapshot, "", key.sort, Some(folder));
    }
    match key.section {
        LibrarySection::All => search_and_sort(snapshot, "", key.sort, None),
        LibrarySection::Home => {
            let mut newest = search_and_sort(snapshot, "", SortMode::Newest, None);
            newest.truncate(12);
            newest
        }
        LibrarySection::Recent => key
            .recent_documents
            .iter()
            .filter_map(|path| find_pdf(snapshot, path).cloned())
            .take(50)
            .collect(),
    }
}

fn scan_destination(input: LibraryViewInput<'_>) -> Option<PathBuf> {
    input
        .selected_folder
        .filter(|selected| {
            *selected != input.library_root
                && input
                    .snapshot
                    .folders
                    .iter()
                    .any(|folder| folder.path.as_path() == *selected)
        })
        .or(input.preferred_scan_folder.filter(|preferred| {
            input
                .snapshot
                .folders
                .iter()
                .any(|folder| folder.path.as_path() == *preferred)
        }))
        .or_else(|| {
            (input.selected_folder.is_none() && input.snapshot.folders.len() == 1)
                .then(|| input.snapshot.folders[0].path.as_path())
        })
        .map(Path::to_path_buf)
}

fn handle_keyboard(
    ui: &egui::Ui,
    state: &mut LibraryViewState,
    enabled: bool,
    documents: &[PdfInfo],
) {
    if !enabled {
        return;
    }
    let search_has_focus = ui.memory(|memory| memory.focused() == Some(Id::new("library-search")));
    let focus_search = ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::F));
    if focus_search {
        state.search_focus_requested = true;
    }
    if !search_has_focus
        && ui.input(|input| input.modifiers.command && input.key_pressed(egui::Key::A))
    {
        state.selected = documents.iter().map(|pdf| pdf.path.clone()).collect();
        state.anchor = documents.first().map(|pdf| pdf.path.clone());
    }
    if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
        if !state.query.is_empty() {
            state.query.clear();
        } else {
            state.clear_selection();
        }
    }
}

fn keyboard_actions(ui: &egui::Ui, state: &LibraryViewState, enabled: bool) -> Vec<LibraryAction> {
    if !enabled {
        return Vec::new();
    }
    if ui.memory(|memory| memory.focused().is_some()) {
        return Vec::new();
    }
    let selected = state.selected_paths();
    let mut actions = Vec::new();
    // Enter only confirms an explicit selection — with nothing selected it
    // must not launch whatever happens to sort first.
    if ui.input(|input| input.key_pressed(egui::Key::Enter))
        && let Some(path) = selected.first().cloned()
    {
        actions.push(LibraryAction::Open(path));
    }
    if selected.len() == 1 && ui.input(|input| input.key_pressed(egui::Key::F2)) {
        actions.push(LibraryAction::Rename(selected[0].clone()));
    }
    if !selected.is_empty() && ui.input(|input| input.key_pressed(egui::Key::Delete)) {
        actions.push(LibraryAction::Recycle(selected));
    }
    actions
}

fn find_pdf<'a>(snapshot: &'a LibrarySnapshot, path: &Path) -> Option<&'a PdfInfo> {
    snapshot.pdfs.iter().find(|pdf| pdf.path == path)
}

fn show_sort_picker(ui: &mut egui::Ui, state: &mut LibraryViewState) {
    egui::ComboBox::from_id_salt("library-sort")
        .selected_text(sort_label(state.sort))
        .width(160.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut state.sort, SortMode::Newest, "Najnowsze");
            ui.selectable_value(&mut state.sort, SortMode::Name, "Nazwa A–Z");
            ui.selectable_value(&mut state.sort, SortMode::Folder, "Folder");
            ui.selectable_value(&mut state.sort, SortMode::Size, "Największe");
        });
}

fn sort_label(sort: SortMode) -> &'static str {
    match sort {
        SortMode::Newest => "Najnowsze",
        SortMode::Name => "Nazwa A–Z",
        SortMode::Folder => "Folder",
        SortMode::Size => "Największe",
    }
}

fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.0} KB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn format_relative_time(modified_secs: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format_relative_time_at(modified_secs, now)
}

fn format_relative_time_at(modified_secs: u64, now_secs: u64) -> String {
    if modified_secs == 0 {
        return "—".to_owned();
    }
    let age = now_secs.saturating_sub(modified_secs);
    match age {
        0..=59 => "przed chwilą".to_owned(),
        60..=3_599 => format!("{} min temu", age / 60),
        3_600..=86_399 => format!("{} godz. temu", age / 3_600),
        86_400..=172_799 => "1 dzień temu".to_owned(),
        _ => format!("{} dni temu", age / 86_400),
    }
}

fn empty_state(ui: &mut egui::Ui, title: &str, detail: &str) {
    Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, Color32::from_gray(222)))
        .corner_radius(10.0)
        .inner_margin(28)
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(19.0).strong());
                ui.label(RichText::new(detail).color(MUTED_TEXT));
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FolderInfo;

    fn pdf(name: &str, folder: &Path, modified_secs: u64) -> PdfInfo {
        PdfInfo {
            name: name.to_owned(),
            path: folder.join(name),
            folder_name: folder
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            folder_path: folder.to_path_buf(),
            size_bytes: 100,
            modified_secs,
        }
    }

    #[test]
    fn formats_document_sizes_for_office_lists() {
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(2 * 1024), "2 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn formats_relative_time_without_clock_dependency() {
        assert_eq!(format_relative_time_at(1_000, 1_010), "przed chwilą");
        assert_eq!(format_relative_time_at(1_000, 1_120), "2 min temu");
        assert_eq!(format_relative_time_at(0, 1_120), "—");
        assert_eq!(format_relative_time_at(1_000, 87_400), "1 dzień temu");
    }

    #[test]
    fn sidebar_labels_are_shortened_without_breaking_unicode() {
        assert_eq!(ellipsize("Dokumenty", 12), "Dokumenty");
        assert_eq!(ellipsize("Zażółć gęślą jaźń", 10), "Zażółć gę…");
    }

    #[test]
    fn document_columns_keep_the_name_flexible_on_narrow_windows() {
        let narrow = TableColumns::new(500.0, true, 10.0);
        assert!(!narrow.show_folder);
        assert!(!narrow.show_size);
        assert!(narrow.compact_folder);
        assert!(narrow.name >= 300.0);

        let wide = TableColumns::new(900.0, true, 10.0);
        assert!(wide.show_folder);
        assert!(wide.show_size);
        assert!(!wide.compact_folder);
    }

    #[test]
    fn home_is_newest_while_recent_preserves_open_order() {
        let folder = PathBuf::from("Akta");
        let old = pdf("A01.pdf", &folder, 10);
        let new = pdf("A02.pdf", &folder, 20);
        let snapshot = LibrarySnapshot {
            root: PathBuf::from("library"),
            folders: Vec::new(),
            pdfs: vec![old.clone(), new.clone()],
        };
        let mut key = VisibleCacheKey {
            snapshot_revision: 1,
            query: String::new(),
            sort: SortMode::Name,
            section: LibrarySection::Home,
            folder_scope: None,
            recent_documents: vec![old.path.clone()],
        };

        let home = visible_documents_for_key(&snapshot, &key);
        assert_eq!(home[0].path, new.path);
        key.section = LibrarySection::Recent;
        let recent = visible_documents_for_key(&snapshot, &key);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].path, old.path);
    }

    #[test]
    fn search_inside_a_folder_stays_scoped_to_that_folder() {
        let folder_a = PathBuf::from("library").join("Klient A");
        let folder_b = PathBuf::from("library").join("Klient B");
        let ours = pdf("A01.pdf", &folder_a, 10);
        let theirs = pdf("A01.pdf", &folder_b, 20);
        let snapshot = LibrarySnapshot {
            root: PathBuf::from("library"),
            folders: Vec::new(),
            pdfs: vec![ours.clone(), theirs.clone()],
        };
        let mut key = VisibleCacheKey {
            snapshot_revision: 1,
            query: "A01".to_owned(),
            sort: SortMode::Name,
            section: LibrarySection::All,
            folder_scope: Some(folder_a.clone()),
            recent_documents: Vec::new(),
        };

        let scoped = visible_documents_for_key(&snapshot, &key);
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].path, ours.path);

        key.folder_scope = None;
        let global = visible_documents_for_key(&snapshot, &key);
        assert_eq!(global.len(), 2);
    }

    #[test]
    fn preferred_scan_folder_is_used_outside_folder_view() {
        let folder_path = PathBuf::from("library").join("Akta");
        let snapshot = LibrarySnapshot {
            root: PathBuf::from("library"),
            folders: vec![FolderInfo {
                name: "Akta".to_owned(),
                path: folder_path.clone(),
                pdf_count: 0,
                modified_secs: 0,
            }],
            pdfs: Vec::new(),
        };
        let input = LibraryViewInput {
            snapshot: &snapshot,
            snapshot_revision: 1,
            recent_documents: &[],
            selected_folder: None,
            preferred_scan_folder: Some(&folder_path),
            library_root: &snapshot.root,
            loading: false,
            keyboard_enabled: true,
        };

        assert_eq!(scan_destination(input), Some(folder_path));
    }

    #[test]
    fn library_root_scope_never_becomes_a_new_scan_destination() {
        let root = PathBuf::from("library");
        let preferred = root.join("Akta");
        let snapshot = LibrarySnapshot {
            root: root.clone(),
            folders: vec![FolderInfo {
                name: "Akta".to_owned(),
                path: preferred.clone(),
                pdf_count: 0,
                modified_secs: 0,
            }],
            pdfs: Vec::new(),
        };
        let input = LibraryViewInput {
            snapshot: &snapshot,
            snapshot_revision: 1,
            recent_documents: &[],
            selected_folder: Some(&root),
            preferred_scan_folder: Some(&preferred),
            library_root: &root,
            loading: false,
            keyboard_enabled: true,
        };

        assert_eq!(scan_destination(input), Some(preferred));
    }

    #[test]
    fn invalid_selected_folder_never_falls_back_to_a_different_folder() {
        let root = PathBuf::from("library");
        let existing = root.join("Stary");
        let selected = root.join("Nowy");
        let snapshot = LibrarySnapshot {
            root: root.clone(),
            folders: vec![FolderInfo {
                name: "Stary".to_owned(),
                path: existing,
                pdf_count: 0,
                modified_secs: 0,
            }],
            pdfs: Vec::new(),
        };
        let input = LibraryViewInput {
            snapshot: &snapshot,
            snapshot_revision: 1,
            recent_documents: &[],
            selected_folder: Some(&selected),
            preferred_scan_folder: None,
            library_root: &root,
            loading: true,
            keyboard_enabled: true,
        };

        assert_eq!(scan_destination(input), None);
    }

    #[test]
    fn visible_select_all_keeps_hidden_selection() {
        let folder = PathBuf::from("Akta");
        let visible = vec![pdf("A01.pdf", &folder, 10), pdf("A02.pdf", &folder, 20)];
        let hidden = folder.join("Ukryty.pdf");
        let mut state = LibraryViewState::default();
        state.selected.insert(hidden.clone());

        set_visible_selection(&mut state, &visible, true);
        assert_eq!(state.selected.len(), 3);
        assert!(state.selected.contains(&hidden));

        set_visible_selection(&mut state, &visible, false);
        assert_eq!(state.selected, BTreeSet::from([hidden]));
        assert!(state.anchor.is_none());
    }
}
