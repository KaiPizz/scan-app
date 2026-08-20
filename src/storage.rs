use crate::document::ColorMode;
use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

static NEXT_FOLDER_RENAME_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct FolderInfo {
    pub name: String,
    pub path: PathBuf,
    pub pdf_count: usize,
    pub modified_secs: u64,
}

#[derive(Debug, Clone)]
pub struct PdfInfo {
    pub name: String,
    pub path: PathBuf,
    pub folder_name: String,
    pub folder_path: PathBuf,
    pub size_bytes: u64,
    pub modified_secs: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    pub library_root: Option<PathBuf>,
    pub last_folder: Option<String>,
    #[serde(default)]
    pub last_folder_path: Option<PathBuf>,
    #[serde(default)]
    pub recent_documents: Vec<PathBuf>,
    #[serde(default)]
    pub auto_capture: Option<bool>,
    #[serde(default)]
    pub backend_url: Option<String>,
    #[serde(default)]
    pub salon_id: Option<String>,
    #[serde(default)]
    pub scan_api_key: Option<String>,
    /// `None` means the default, black-and-white (G4) pages.
    #[serde(default)]
    pub color_mode: Option<ColorMode>,
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
    load_settings_file(&path)
        .or_else(|| load_settings_file(&settings_backup_path(&path)))
        .unwrap_or_default()
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let path = settings_path().ok_or_else(|| "Nie można znaleźć folderu ustawień.".to_owned())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(display_io_error)?;
    }
    let contents = ron::to_string(settings).map_err(|error| error.to_string())?;
    if let Ok(previous) = fs::read_to_string(&path)
        && ron::from_str::<Settings>(&previous).is_ok()
    {
        crate::atomic_file::write(&settings_backup_path(&path), previous)
            .map_err(display_io_error)?;
    }
    crate::atomic_file::write(&path, contents).map_err(display_io_error)
}

fn load_settings_file(path: &Path) -> Option<Settings> {
    let contents = fs::read_to_string(path).ok()?;
    ron::from_str(&contents).ok()
}

fn settings_backup_path(path: &Path) -> PathBuf {
    let mut backup = PathBuf::from(path.as_os_str());
    backup.as_mut_os_string().push(".bak");
    backup
}

pub fn ensure_library(root: &Path) -> Result<(), String> {
    fs::create_dir_all(root).map_err(display_io_error)
}

#[cfg(test)]
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
            modified_secs: entry
                .metadata()
                .ok()
                .as_ref()
                .map_or(0, metadata_modified_secs),
        });
    }
    folders.sort_by_key(|folder| folder.name.to_lowercase());
    Ok(folders)
}

/// Scans the library root and its direct child folders exactly once each.
///
/// This is the production indexing path. Nested folders are intentionally not
/// traversed, and PDF contents are never opened.
pub fn scan_library_metadata(root: &Path) -> Result<(Vec<FolderInfo>, Vec<PdfInfo>), String> {
    ensure_library(root)?;
    let root_name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut folders = Vec::new();
    let mut pdfs = Vec::new();

    for entry in fs::read_dir(root).map_err(display_io_error)? {
        let entry = entry.map_err(display_io_error)?;
        let file_type = entry.file_type().map_err(display_io_error)?;
        if file_type.is_dir() {
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let path = entry.path();
            let folder_pdfs = list_pdfs(&path)?;
            folders.push(FolderInfo {
                name: entry.file_name().to_string_lossy().into_owned(),
                path,
                pdf_count: folder_pdfs.len(),
                modified_secs: entry
                    .metadata()
                    .ok()
                    .as_ref()
                    .map_or(0, metadata_modified_secs),
            });
            pdfs.extend(folder_pdfs);
        } else if let Some(pdf) = pdf_info_from_entry(&entry, &root_name, root)? {
            pdfs.push(pdf);
        }
    }

    Ok((folders, pdfs))
}

pub fn list_pdfs(folder: &Path) -> Result<Vec<PdfInfo>, String> {
    let mut files = Vec::new();
    let folder_name = folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for entry in fs::read_dir(folder).map_err(display_io_error)? {
        let entry = entry.map_err(display_io_error)?;
        if let Some(pdf) = pdf_info_from_entry(&entry, &folder_name, folder)? {
            files.push(pdf);
        }
    }
    files.sort_by_key(|file| file.name.to_lowercase());
    Ok(files)
}

fn pdf_info_from_entry(
    entry: &fs::DirEntry,
    folder_name: &str,
    folder_path: &Path,
) -> Result<Option<PdfInfo>, String> {
    let file_type = entry.file_type().map_err(display_io_error)?;
    let path = entry.path();
    if !file_type.is_file() || !has_pdf_extension(&path) {
        return Ok(None);
    }
    // Dot-prefixed files are this app's own recovery/lock artifacts
    // (`.X.replace-recovery-*.pdf`, `.X.scan-save-conflict-*.pdf`, locks) —
    // never documents. The startup sweep surfaces or removes them.
    if entry.file_name().to_string_lossy().starts_with('.') {
        return Ok(None);
    }
    let metadata = entry.metadata().ok();
    Ok(Some(PdfInfo {
        name: entry.file_name().to_string_lossy().into_owned(),
        path,
        folder_name: folder_name.to_owned(),
        folder_path: folder_path.to_path_buf(),
        size_bytes: metadata.as_ref().map_or(0, fs::Metadata::len),
        modified_secs: metadata.as_ref().map_or(0, metadata_modified_secs),
    }))
}

pub fn pdf_info(path: &Path) -> Result<PdfInfo, String> {
    if !is_pdf(path) {
        return Err("Wybrany plik nie jest dostępnym plikiem PDF.".to_owned());
    }
    let folder_path = path
        .parent()
        .ok_or_else(|| "Nie można ustalić folderu pliku PDF.".to_owned())?
        .to_path_buf();
    let metadata = fs::metadata(path).map_err(display_io_error)?;
    Ok(PdfInfo {
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: path.to_path_buf(),
        folder_name: folder_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        folder_path,
        size_bytes: metadata.len(),
        modified_secs: metadata_modified_secs(&metadata),
    })
}

pub fn create_folder(root: &Path, requested_name: &str) -> Result<FolderInfo, String> {
    ensure_library(root)?;
    let name = validate_windows_name(requested_name, "Nazwa folderu")?;
    let path = root.join(&name);
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err("Folder o tej nazwie już istnieje.".to_owned());
        }
        Err(error) => return Err(display_io_error(error)),
    }
    let modified_secs = path_modified_secs(&path);
    Ok(FolderInfo {
        name,
        path,
        pdf_count: 0,
        modified_secs,
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
    let case_only_on_windows = cfg!(windows) && folder.name.to_lowercase() == name.to_lowercase();
    if case_only_on_windows {
        return rename_folder_case_only(folder, name, new_path);
    }
    if new_path.exists() {
        return Err("Folder o tej nazwie już istnieje.".to_owned());
    }
    move_directory_no_clobber(&folder.path, &new_path).map_err(display_io_error)?;
    let modified_secs = path_modified_secs(&new_path);
    Ok(FolderInfo {
        name,
        path: new_path,
        pdf_count: folder.pdf_count,
        modified_secs: if modified_secs == 0 {
            folder.modified_secs
        } else {
            modified_secs
        },
    })
}

fn rename_folder_case_only(
    folder: &FolderInfo,
    name: String,
    new_path: PathBuf,
) -> Result<FolderInfo, String> {
    let parent = folder
        .path
        .parent()
        .ok_or_else(|| "Nie można zmienić nazwy tego folderu.".to_owned())?;
    let temporary = (0..100)
        .map(|_| {
            let id = NEXT_FOLDER_RENAME_ID.fetch_add(1, Ordering::Relaxed);
            parent.join(format!(
                ".skaner-folder-rename-recovery-{}-{id}",
                std::process::id()
            ))
        })
        .find(|path| !path.exists())
        .ok_or_else(|| "Nie można utworzyć nazwy tymczasowej folderu.".to_owned())?;
    move_directory_no_clobber(&folder.path, &temporary).map_err(display_io_error)?;
    if let Err(rename_error) = move_directory_no_clobber(&temporary, &new_path) {
        return match move_directory_no_clobber(&temporary, &folder.path) {
            Ok(()) => Err(format!(
                "Nie można zmienić wielkości liter w nazwie folderu: {rename_error}"
            )),
            Err(rollback_error) => Err(format!(
                "Nie można dokończyć zmiany nazwy. Folder pozostał jako {}. Błąd: {rename_error}; przywracanie: {rollback_error}",
                temporary.display()
            )),
        };
    }
    let modified_secs = path_modified_secs(&new_path);
    Ok(FolderInfo {
        name,
        path: new_path,
        pdf_count: folder.pdf_count,
        modified_secs: modified_secs.max(folder.modified_secs),
    })
}

fn move_directory_no_clobber(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        crate::atomic_file::move_new(source, target)
    }

    #[cfg(not(windows))]
    {
        if target.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "target already exists",
            ));
        }
        fs::rename(source, target)
    }
}

/// Suggests the next filename stem for the strongest trailing-number series.
///
/// A folder containing `A01.pdf`, `A02.pdf`, and `A09.pdf` therefore yields
/// `A10`. Series are grouped by a case-insensitive prefix and digit width, so
/// an unrelated one-off filename such as `Raport2025.pdf` does not normally
/// displace an established sequence. If no usable numeric suffix exists, the
/// trimmed folder name is returned unchanged.
pub fn next_sequence_name(existing: &[PdfInfo], fallback_folder_name: &str) -> String {
    #[derive(Debug)]
    struct Sequence {
        prefix: String,
        width: usize,
        count: usize,
        highest: u64,
        latest_modified: u64,
    }

    let mut sequences: BTreeMap<(String, usize), Sequence> = BTreeMap::new();
    for pdf in existing {
        let stem = pdf
            .name
            .get(pdf.name.len().saturating_sub(4)..)
            .filter(|suffix| suffix.eq_ignore_ascii_case(".pdf"))
            .and_then(|_| pdf.name.get(..pdf.name.len() - 4))
            .unwrap_or(&pdf.name);
        let split_at = stem
            .char_indices()
            .rev()
            .find_map(|(index, character)| {
                (!character.is_ascii_digit()).then_some(index + character.len_utf8())
            })
            .unwrap_or(0);
        let (prefix, digits) = stem.split_at(split_at);
        if digits.is_empty() {
            continue;
        }
        let Ok(number) = digits.parse::<u64>() else {
            continue;
        };
        if number == u64::MAX {
            continue;
        }

        let key = (prefix.to_lowercase(), digits.len());
        let sequence = sequences.entry(key).or_insert_with(|| Sequence {
            prefix: prefix.to_owned(),
            width: digits.len(),
            count: 0,
            highest: number,
            latest_modified: pdf.modified_secs,
        });
        sequence.count += 1;
        if number > sequence.highest
            || (number == sequence.highest && pdf.modified_secs >= sequence.latest_modified)
        {
            sequence.prefix = prefix.to_owned();
            sequence.highest = number;
        }
        sequence.latest_modified = sequence.latest_modified.max(pdf.modified_secs);
    }

    let best = sequences
        .values()
        .filter(|sequence| sequence.count >= 2)
        .max_by(|left, right| {
            left.count
                .cmp(&right.count)
                .then_with(|| left.highest.cmp(&right.highest))
                .then_with(|| left.latest_modified.cmp(&right.latest_modified))
                .then_with(|| left.prefix.to_lowercase().cmp(&right.prefix.to_lowercase()))
        });
    if let Some(sequence) = best {
        let next = sequence.highest + 1;
        return format!(
            "{}{:0width$}",
            sequence.prefix,
            next,
            width = sequence.width
        );
    }

    let fallback = fallback_folder_name.trim();
    if fallback.is_empty() {
        "Dokument".to_owned()
    } else {
        fallback.to_owned()
    }
}

pub fn unique_pdf_path(folder: &Path, requested_name: &str) -> Result<PathBuf, String> {
    let base = normalized_pdf_stem(requested_name)?;

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

pub fn normalized_pdf_stem(requested_name: &str) -> Result<String, String> {
    let raw = requested_name.trim();
    let without_extension = raw
        .get(raw.len().saturating_sub(4)..)
        .filter(|suffix| suffix.eq_ignore_ascii_case(".pdf"))
        .and_then(|_| raw.get(..raw.len() - 4))
        .unwrap_or(raw);
    validate_windows_name(without_extension, "Nazwa pliku")
}

fn settings_path() -> Option<PathBuf> {
    ProjectDirs::from("pl", "SkanerDokumentow", "Skaner dokumentów")
        .map(|dirs| dirs.config_dir().join("ustawienia.ron"))
}

/// Held for the whole process lifetime. Two instances would share one
/// crash-recovery session directory and silently race its manifest, so a
/// second instance must not start.
pub struct InstanceLock {
    _file: fs::File,
}

/// `Ok(Some(_))` — lock acquired. `Ok(None)` — locking unavailable (missing
/// profile dir, exotic filesystem error): run anyway rather than block the
/// scanner. `Err(_)` — another instance definitively holds the lock.
pub fn acquire_instance_lock() -> Result<Option<InstanceLock>, String> {
    let Some(dirs) = ProjectDirs::from("pl", "SkanerDokumentow", "Skaner dokumentów") else {
        return Ok(None);
    };
    let dir = dirs.data_dir();
    if fs::create_dir_all(dir).is_err() {
        return Ok(None);
    }
    let path = dir.join("instancja.lock");

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        const ERROR_SHARING_VIOLATION: i32 = 32;
        match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(&path)
        {
            Ok(file) => Ok(Some(InstanceLock { _file: file })),
            Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => {
                Err("Skaner dokumentów jest już uruchomiony na tym komputerze.".to_owned())
            }
            Err(_) => Ok(None),
        }
    }

    #[cfg(not(windows))]
    {
        // Non-Windows builds exist only for tests; a plain open keeps the
        // code path exercised without real exclusivity.
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map(|file| Some(InstanceLock { _file: file }))
            .or(Ok(None))
    }
}

fn is_pdf(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file())
        && has_pdf_extension(path)
}

fn has_pdf_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

pub fn validate_windows_name(requested_name: &str, label: &str) -> Result<String, String> {
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
        || matches!(stem, "COM¹" | "COM²" | "COM³" | "LPT¹" | "LPT²" | "LPT³")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        return Err(format!("{label} jest zarezerwowana przez system Windows."));
    }
    Ok(name.to_owned())
}

fn path_modified_secs(path: &Path) -> u64 {
    fs::metadata(path)
        .ok()
        .as_ref()
        .map_or(0, metadata_modified_secs)
}

fn metadata_modified_secs(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs())
}

fn display_io_error(error: std::io::Error) -> String {
    format!("Błąd systemu plików: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn create(label: &str) -> Self {
            let serial = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "skaner-storage-{label}-{}-{serial}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pdf_info(name: &str, modified_secs: u64) -> PdfInfo {
        PdfInfo {
            name: name.to_owned(),
            path: PathBuf::from(name),
            folder_name: "Akta".to_owned(),
            folder_path: PathBuf::from("Akta"),
            size_bytes: 0,
            modified_secs,
        }
    }

    #[test]
    fn rejects_invalid_windows_names() {
        assert!(validate_windows_name("", "Nazwa").is_err());
        assert!(validate_windows_name("CON", "Nazwa").is_err());
        assert!(validate_windows_name("COM¹.txt", "Nazwa").is_err());
        assert!(validate_windows_name("a/b", "Nazwa").is_err());
        assert!(validate_windows_name("poprawna nazwa", "Nazwa").is_ok());
    }

    #[test]
    fn normalizes_mixed_case_pdf_extension() {
        assert_eq!(normalized_pdf_stem("Raport.PdF").unwrap(), "Raport");
        assert_eq!(normalized_pdf_stem("Raport.pdf").unwrap(), "Raport");
        assert_eq!(normalized_pdf_stem("Raport").unwrap(), "Raport");
    }

    #[test]
    fn lists_mixed_case_pdf_with_folder_and_file_metadata() {
        let root = TestDirectory::create("metadata");
        let folder = root.0.join("Klient A");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("Umowa.PdF"), b"sample pdf bytes").unwrap();
        fs::write(folder.join("notatka.txt"), b"not a pdf").unwrap();

        let folders = list_folders(&root.0).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "Klient A");
        assert_eq!(folders[0].pdf_count, 1);
        assert!(folders[0].modified_secs > 0);

        let pdfs = list_pdfs(&folder).unwrap();
        assert_eq!(pdfs.len(), 1);
        assert_eq!(pdfs[0].name, "Umowa.PdF");
        assert_eq!(pdfs[0].folder_name, "Klient A");
        assert_eq!(pdfs[0].folder_path, folder);
        assert_eq!(pdfs[0].size_bytes, 16);
        assert!(pdfs[0].modified_secs > 0);
    }

    #[test]
    fn recovery_artifacts_are_not_indexed_as_documents() {
        let root = TestDirectory::create("dotfiles");
        let folder = root.0.join("Akta");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("Umowa.pdf"), b"pdf").unwrap();
        fs::write(folder.join(".Umowa.replace-recovery-1-2.pdf"), b"backup").unwrap();
        fs::create_dir(root.0.join(".skaner-folder-rename-recovery-1-3")).unwrap();

        let (folders, pdfs) = scan_library_metadata(&root.0).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(pdfs.len(), 1);
        assert_eq!(pdfs[0].name, "Umowa.pdf");
    }

    #[test]
    fn renames_folder_when_only_letter_case_changes() {
        let root = TestDirectory::create("folder-case");
        let folder = create_folder(&root.0, "Akta").unwrap();
        fs::write(folder.path.join("A01.pdf"), b"pdf").unwrap();

        let renamed = rename_folder(&folder, "AKTA").unwrap();

        assert_eq!(renamed.name, "AKTA");
        assert!(renamed.path.join("A01.pdf").is_file());
    }

    #[test]
    fn suggests_next_name_from_natural_numeric_series() {
        let existing = vec![
            pdf_info("A01.pdf", 10),
            pdf_info("A02.PDF", 20),
            pdf_info("A09.PdF", 30),
            pdf_info("Raport2025.pdf", 40),
        ];

        assert_eq!(next_sequence_name(&existing, "Akta"), "A10");
        assert_eq!(
            next_sequence_name(&[pdf_info("Raport2025.pdf", 40)], "Akta"),
            "Akta"
        );
        assert_eq!(next_sequence_name(&[], "  Akta klienta  "), "Akta klienta");
    }

    // Uses the real profile directory: fails if the scanner app itself is
    // running while the tests execute — close it first.
    #[cfg(windows)]
    #[test]
    fn second_instance_lock_is_rejected() {
        let first = acquire_instance_lock()
            .expect("first acquire")
            .expect("first lock");
        assert!(
            acquire_instance_lock().is_err(),
            "druga instancja musi zostać odrzucona"
        );
        drop(first);
        assert!(acquire_instance_lock().expect("re-acquire").is_some());
    }

    #[test]
    fn color_mode_defaults_to_black_white_and_round_trips() {
        let legacy: Settings = ron::from_str("(library_root:None,last_folder:None)").unwrap();
        assert_eq!(legacy.color_mode, None);
        assert_eq!(legacy.color_mode.unwrap_or_default(), ColorMode::BlackWhite);
        let settings = Settings {
            color_mode: Some(ColorMode::Color),
            ..Settings::default()
        };
        let text = ron::to_string(&settings).unwrap();
        let back: Settings = ron::from_str(&text).unwrap();
        assert_eq!(back.color_mode, Some(ColorMode::Color));
    }

    #[test]
    fn legacy_settings_load_with_empty_library_history() {
        let settings: Settings = ron::from_str(
            "(library_root:None,last_folder:Some(\"Akta\"),auto_capture:Some(false))",
        )
        .unwrap();

        assert_eq!(settings.last_folder.as_deref(), Some("Akta"));
        assert!(settings.last_folder_path.is_none());
        assert!(settings.recent_documents.is_empty());
    }
}
