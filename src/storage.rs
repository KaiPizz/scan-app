use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub name: String,
    pub path: PathBuf,
    pub pdf_count: usize,
}

#[derive(Debug, Clone)]
pub struct PdfInfo {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    pub library_root: Option<PathBuf>,
    pub last_folder: Option<String>,
    #[serde(default)]
    pub auto_capture: Option<bool>,
}

pub fn default_library_root() -> PathBuf {
    UserDirs::new()
        .and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("Dokumenty"))
        .join("Zeskanowane dokumenty")
}

pub fn load_settings() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    let Ok(contents) = fs::read_to_string(path) else {
        return Settings::default();
    };
    ron::from_str(&contents).unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "Nie można znaleźć folderu ustawień.".to_owned())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(display_io_error)?;
    }
    let contents = ron::to_string(settings).map_err(|error| error.to_string())?;
    fs::write(path, contents).map_err(display_io_error)
}

pub fn ensure_library(root: &Path) -> Result<(), String> {
    fs::create_dir_all(root).map_err(display_io_error)
}

pub fn list_folders(root: &Path) -> Result<Vec<FolderInfo>, String> {
    ensure_library(root)?;
    let mut folders = Vec::new();
    for entry in fs::read_dir(root).map_err(display_io_error)? {
        let entry = entry.map_err(display_io_error)?;
        let file_type = entry.file_type().map_err(display_io_error)?;
        if !file_type.is_dir() {
            continue;
        }
        let path = entry.path();
        let pdf_count = fs::read_dir(&path)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|item| is_pdf(&item.path()))
            .count();
        folders.push(FolderInfo {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
            pdf_count,
        });
    }
    folders.sort_by_key(|folder| folder.name.to_lowercase());
    Ok(folders)
}

pub fn list_pdfs(folder: &Path) -> Result<Vec<PdfInfo>, String> {
    let mut files = Vec::new();
    for entry in fs::read_dir(folder).map_err(display_io_error)? {
        let entry = entry.map_err(display_io_error)?;
        let path = entry.path();
        if !is_pdf(&path) {
            continue;
        }
        files.push(PdfInfo {
            name: entry.file_name().to_string_lossy().into_owned(),
            path,
        });
    }
    files.sort_by_key(|file| file.name.to_lowercase());
    Ok(files)
}

pub fn create_folder(root: &Path, requested_name: &str) -> Result<FolderInfo, String> {
    let name = validate_windows_name(requested_name, "Nazwa folderu")?;
    let path = root.join(&name);
    if path.exists() {
        return Err("Folder o tej nazwie już istnieje.".to_owned());
    }
    fs::create_dir_all(&path).map_err(display_io_error)?;
    Ok(FolderInfo {
        name,
        path,
        pdf_count: 0,
    })
}

pub fn rename_folder(folder: &FolderInfo, requested_name: &str) -> Result<FolderInfo, String> {
    let name = validate_windows_name(requested_name, "Nazwa folderu")?;
    if name == folder.name {
        return Ok(folder.clone());
    }
    let parent = folder
        .path
        .parent()
        .ok_or_else(|| "Nie można zmienić nazwy tego folderu.".to_owned())?;
    let new_path = parent.join(&name);
    if new_path.exists() {
        return Err("Folder o tej nazwie już istnieje.".to_owned());
    }
    fs::rename(&folder.path, &new_path).map_err(display_io_error)?;
    Ok(FolderInfo {
        name,
        path: new_path,
        pdf_count: folder.pdf_count,
    })
}

pub fn unique_pdf_path(folder: &Path, requested_name: &str) -> Result<PathBuf, String> {
    let raw = requested_name.trim();
    let without_extension = raw
        .strip_suffix(".pdf")
        .or_else(|| raw.strip_suffix(".PDF"))
        .unwrap_or(raw);
    let base = validate_windows_name(without_extension, "Nazwa pliku")?;

    let initial = folder.join(format!("{base}.pdf"));
    if !initial.exists() {
        return Ok(initial);
    }
    for number in 2..10_000 {
        let candidate = folder.join(format!("{base} ({number}).pdf"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("Nie można utworzyć unikalnej nazwy pliku.".to_owned())
}

fn settings_path() -> Option<PathBuf> {
    ProjectDirs::from("pl", "SkanerDokumentow", "Skaner dokumentów")
        .map(|dirs| dirs.config_dir().join("ustawienia.ron"))
}

fn is_pdf(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn validate_windows_name(requested_name: &str, label: &str) -> Result<String, String> {
    let name = requested_name.trim();
    if name.is_empty() {
        return Err(format!("{label} nie może być pusta."));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(format!("{label} nie może kończyć się kropką ani spacją."));
    }
    if name.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
    }) {
        return Err(format!("{label} zawiera niedozwolony znak."));
    }
    let uppercase = name.to_ascii_uppercase();
    let stem = uppercase.split('.').next().unwrap_or_default();
    let reserved = matches!(stem, "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        return Err(format!("{label} jest zarezerwowana przez system Windows."));
    }
    Ok(name.to_owned())
}

fn display_io_error(error: std::io::Error) -> String {
    format!("Błąd systemu plików: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_windows_names() {
        assert!(validate_windows_name("", "Nazwa").is_err());
        assert!(validate_windows_name("CON", "Nazwa").is_err());
        assert!(validate_windows_name("a/b", "Nazwa").is_err());
        assert!(validate_windows_name("poprawna nazwa", "Nazwa").is_ok());
    }
}
