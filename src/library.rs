use crate::storage::{
    FolderInfo, PdfInfo, normalized_pdf_stem, scan_library_metadata, unique_pdf_path,
};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Default)]
pub struct LibrarySnapshot {
    pub root: PathBuf,
    pub folders: Vec<FolderInfo>,
    pub pdfs: Vec<PdfInfo>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortMode {
    #[default]
    Modified,
    Name,
    Folder,
    Size,
}

impl SortMode {
    /// The direction users expect when first picking a column: texts A→Z,
    /// dates newest-first, sizes largest-first.
    pub fn default_ascending(self) -> bool {
        matches!(self, Self::Name | Self::Folder)
    }
}

#[derive(Debug)]
pub enum LibraryEvent {
    Refreshed {
        generation: u64,
        snapshot: LibrarySnapshot,
    },
    Failed {
        generation: u64,
        error: String,
    },
}

impl LibraryEvent {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Refreshed { generation, .. } | Self::Failed { generation, .. } => *generation,
        }
    }
}

enum WorkerCommand {
    Refresh { generation: u64, root: PathBuf },
    Stop,
}

/// Owns one lightweight filesystem-indexing thread.
///
/// Refreshes are tagged with a monotonically increasing generation. Callers only receive an
/// event for the most recently requested generation, so a slow scan can never replace a newer
/// library root or query state in the UI.
pub struct LibraryWorker {
    command_tx: Sender<WorkerCommand>,
    event_rx: Receiver<LibraryEvent>,
    thread: Option<JoinHandle<()>>,
    next_generation: u64,
    latest_requested_generation: u64,
}

impl LibraryWorker {
    pub fn start() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("library-index".to_owned())
            .spawn(move || worker_loop(command_rx, event_tx))
            .expect("cannot start library indexing thread");

        Self {
            command_tx,
            event_rx,
            thread: Some(thread),
            next_generation: 1,
            latest_requested_generation: 0,
        }
    }

    pub fn request_refresh(&mut self, root: impl Into<PathBuf>) -> Result<u64, String> {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.command_tx
            .send(WorkerCommand::Refresh {
                generation,
                root: root.into(),
            })
            .map_err(|_| "Indeks biblioteki został zatrzymany.".to_owned())?;
        self.latest_requested_generation = generation;
        Ok(generation)
    }

    /// Returns only an event matching the latest request and silently drains stale generations.
    pub fn try_recv_latest(&self) -> Option<LibraryEvent> {
        let mut latest = None;
        loop {
            match self.event_rx.try_recv() {
                Ok(event) if event.generation() == self.latest_requested_generation => {
                    latest = Some(event);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return latest,
            }
        }
    }
}

impl Default for LibraryWorker {
    fn default() -> Self {
        Self::start()
    }
}

impl Drop for LibraryWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(WorkerCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug)]
pub enum FileOperationEvent {
    Moved(FileActionReport),
    Recycled(FileActionReport),
}

enum FileOperationCommand {
    Move {
        sources: Vec<PathBuf>,
        target_folder: PathBuf,
    },
    Recycle {
        sources: Vec<PathBuf>,
    },
    Stop,
}

/// Serializes potentially slow Explorer-style file operations off the UI thread.
pub struct FileOperationWorker {
    command_tx: Sender<FileOperationCommand>,
    event_rx: Receiver<FileOperationEvent>,
    thread: Option<JoinHandle<()>>,
}

impl FileOperationWorker {
    pub fn start() -> Self {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("library-file-operations".to_owned())
            .spawn(move || file_operation_loop(command_rx, event_tx))
            .expect("cannot start library file-operation thread");
        Self {
            command_tx,
            event_rx,
            thread: Some(thread),
        }
    }

    pub fn request_move(
        &self,
        sources: Vec<PathBuf>,
        target_folder: PathBuf,
    ) -> Result<(), String> {
        self.command_tx
            .send(FileOperationCommand::Move {
                sources,
                target_folder,
            })
            .map_err(|_| "Obsługa plików została zatrzymana.".to_owned())
    }

    pub fn request_recycle(&self, sources: Vec<PathBuf>) -> Result<(), String> {
        self.command_tx
            .send(FileOperationCommand::Recycle { sources })
            .map_err(|_| "Obsługa plików została zatrzymana.".to_owned())
    }

    pub fn try_recv(&self) -> Result<Option<FileOperationEvent>, String> {
        match self.event_rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                Err("Obsługa plików została nieoczekiwanie zatrzymana.".to_owned())
            }
        }
    }
}

impl Default for FileOperationWorker {
    fn default() -> Self {
        Self::start()
    }
}

impl Drop for FileOperationWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.send(FileOperationCommand::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn file_operation_loop(
    command_rx: Receiver<FileOperationCommand>,
    event_tx: Sender<FileOperationEvent>,
) {
    while let Ok(command) = command_rx.recv() {
        let event = match command {
            FileOperationCommand::Move {
                sources,
                target_folder,
            } => FileOperationEvent::Moved(move_pdfs_keep_both(&sources, &target_folder)),
            FileOperationCommand::Recycle { sources } => {
                FileOperationEvent::Recycled(recycle_pdfs(&sources))
            }
            FileOperationCommand::Stop => return,
        };
        if event_tx.send(event).is_err() {
            return;
        }
    }
}

fn worker_loop(command_rx: Receiver<WorkerCommand>, event_tx: Sender<LibraryEvent>) {
    while let Ok(command) = command_rx.recv() {
        let (mut generation, mut root) = match command {
            WorkerCommand::Refresh { generation, root } => (generation, root),
            WorkerCommand::Stop => return,
        };

        // Coalesce refreshes that queued while the worker was idle.
        loop {
            match command_rx.try_recv() {
                Ok(WorkerCommand::Refresh {
                    generation: newer_generation,
                    root: newer_root,
                }) => {
                    generation = newer_generation;
                    root = newer_root;
                }
                Ok(WorkerCommand::Stop) => return,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        let event = match scan_library(&root) {
            Ok(snapshot) => LibraryEvent::Refreshed {
                generation,
                snapshot,
            },
            Err(error) => LibraryEvent::Failed { generation, error },
        };
        if event_tx.send(event).is_err() {
            return;
        }
    }
}

/// Reads only directory entries and file metadata. PDF contents are intentionally never opened.
pub fn scan_library(root: &Path) -> Result<LibrarySnapshot, String> {
    let (mut folders, mut pdfs) = scan_library_metadata(root)?;
    folders.sort_by(|left, right| natural_cmp(&left.name, &right.name));
    sort_pdfs(&mut pdfs, SortMode::Name, true);
    Ok(LibrarySnapshot {
        root: root.to_path_buf(),
        folders,
        pdfs,
    })
}

pub fn search_and_sort(
    snapshot: &LibrarySnapshot,
    query: &str,
    sort_mode: SortMode,
    ascending: bool,
    folder_scope: Option<&Path>,
) -> Vec<PdfInfo> {
    let normalized_query = normalize_search_text(query);
    let tokens: Vec<&str> = normalized_query.split_whitespace().collect();
    let mut result: Vec<PdfInfo> = snapshot
        .pdfs
        .iter()
        .filter(|pdf| {
            folder_scope.is_none_or(|folder| pdf.folder_path == folder)
                && search_matches(pdf, &tokens)
        })
        .cloned()
        .collect();
    sort_pdfs(&mut result, sort_mode, ascending);
    result
}

/// `ascending` flips only the primary key; tie-breakers always run in their
/// natural ascending order so reversing a sort never scrambles equal rows.
pub fn sort_pdfs(pdfs: &mut [PdfInfo], sort_mode: SortMode, ascending: bool) {
    if pdfs.len() < 2 {
        return;
    }
    struct SortEntry {
        pdf: PdfInfo,
        name: String,
        folder: String,
    }
    let mut entries: Vec<SortEntry> = pdfs
        .iter()
        .cloned()
        .map(|pdf| SortEntry {
            name: normalize_search_text(&pdf.name),
            folder: normalize_search_text(&pdf.folder_name),
            pdf,
        })
        .collect();
    let natural_name = |left: &SortEntry, right: &SortEntry| {
        natural_cmp_normalized(&left.name, &right.name)
            .then_with(|| left.pdf.name.cmp(&right.pdf.name))
    };
    let natural_folder = |left: &SortEntry, right: &SortEntry| {
        natural_cmp_normalized(&left.folder, &right.folder)
            .then_with(|| left.pdf.folder_name.cmp(&right.pdf.folder_name))
    };
    entries.sort_by(|left, right| {
        let primary = match sort_mode {
            SortMode::Modified => left.pdf.modified_secs.cmp(&right.pdf.modified_secs),
            SortMode::Name => natural_name(left, right),
            SortMode::Folder => natural_folder(left, right),
            SortMode::Size => left.pdf.size_bytes.cmp(&right.pdf.size_bytes),
        };
        let primary = if ascending { primary } else { primary.reverse() };
        primary
            .then_with(|| match sort_mode {
                SortMode::Modified | SortMode::Size => {
                    natural_name(left, right).then_with(|| natural_folder(left, right))
                }
                SortMode::Name => natural_folder(left, right),
                SortMode::Folder => natural_name(left, right),
            })
            .then_with(|| left.pdf.path.cmp(&right.pdf.path))
    });
    for (target, entry) in pdfs.iter_mut().zip(entries) {
        *target = entry.pdf;
    }
}

fn search_matches(pdf: &PdfInfo, tokens: &[&str]) -> bool {
    if tokens.is_empty() {
        return true;
    }
    let haystack = normalize_search_text(&format!("{} {}", pdf.name, pdf.folder_name));
    tokens.iter().all(|token| haystack.contains(token))
}

pub fn normalize_search_text(text: &str) -> String {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter_map(|character| match character {
            'ą' => Some('a'),
            'ć' => Some('c'),
            'ę' => Some('e'),
            'ł' => Some('l'),
            'ń' => Some('n'),
            'ó' => Some('o'),
            'ś' => Some('s'),
            'ź' | 'ż' => Some('z'),
            '\u{0300}'..='\u{036f}' => None,
            other => Some(other),
        })
        .collect()
}

pub fn natural_cmp(left: &str, right: &str) -> Ordering {
    let left_normalized = normalize_search_text(left);
    let right_normalized = normalize_search_text(right);
    natural_cmp_normalized(&left_normalized, &right_normalized).then_with(|| left.cmp(right))
}

fn natural_cmp_normalized(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut left_index = 0;
    let mut right_index = 0;

    while left_index < left.len() && right_index < right.len() {
        let numbers = left[left_index].is_ascii_digit() && right[right_index].is_ascii_digit();
        if numbers {
            let left_end = digit_run_end(left, left_index);
            let right_end = digit_run_end(right, right_index);
            let ordering =
                compare_digit_runs(&left[left_index..left_end], &right[right_index..right_end]);
            if ordering != Ordering::Equal {
                return ordering;
            }
            left_index = left_end;
            right_index = right_end;
        } else {
            let left_end = non_digit_run_end(left, left_index);
            let right_end = non_digit_run_end(right, right_index);
            let ordering = left[left_index..left_end].cmp(&right[right_index..right_end]);
            if ordering != Ordering::Equal {
                return ordering;
            }
            left_index = left_end;
            right_index = right_end;
        }
    }
    left.len().cmp(&right.len())
}

fn digit_run_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| !byte.is_ascii_digit())
        .map_or(bytes.len(), |offset| start + offset)
}

fn non_digit_run_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(u8::is_ascii_digit)
        .map_or(bytes.len(), |offset| start + offset)
}

fn compare_digit_runs(left: &[u8], right: &[u8]) -> Ordering {
    let left_trimmed = trim_number_zeros(left);
    let right_trimmed = trim_number_zeros(right);
    left_trimmed
        .len()
        .cmp(&right_trimmed.len())
        .then_with(|| left_trimmed.cmp(right_trimmed))
        .then_with(|| left.len().cmp(&right.len()))
}

fn trim_number_zeros(number: &[u8]) -> &[u8] {
    let first_nonzero = number
        .iter()
        .position(|byte| *byte != b'0')
        .unwrap_or(number.len().saturating_sub(1));
    &number[first_nonzero..]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileActionSuccess {
    pub source: PathBuf,
    pub destination: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileActionFailure {
    pub source: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileActionReport {
    pub successes: Vec<FileActionSuccess>,
    pub failures: Vec<FileActionFailure>,
    pub skipped: Vec<PathBuf>,
}

impl FileActionReport {
    pub fn completed_count(&self) -> usize {
        self.successes.len()
    }

    pub fn failed_count(&self) -> usize {
        self.failures.len()
    }

    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }

    #[cfg(test)]
    pub fn all_succeeded(&self) -> bool {
        self.failures.is_empty()
    }

    fn success(&mut self, source: PathBuf, destination: Option<PathBuf>) {
        self.successes.push(FileActionSuccess {
            source,
            destination,
        });
    }

    fn failure(&mut self, source: PathBuf, error: String) {
        self.failures.push(FileActionFailure { source, error });
    }

    fn skip(&mut self, source: PathBuf) {
        self.skipped.push(source);
    }
}

/// Renames the directory entry only; PDF bytes are not read or recompressed.
pub fn rename_pdf(source: &Path, requested_name: &str) -> Result<PathBuf, String> {
    validate_pdf_source(source)?;
    let parent = source
        .parent()
        .ok_or_else(|| "Nie można zmienić nazwy tego pliku.".to_owned())?;
    let stem = normalized_pdf_stem(requested_name)?;
    let target = parent.join(format!("{stem}.pdf"));
    if source == target {
        return Ok(target);
    }

    let case_only_on_windows = cfg!(windows)
        && source
            .file_name()
            .zip(target.file_name())
            .is_some_and(|(left, right)| {
                left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
            });

    if case_only_on_windows {
        return rename_case_only(source, &target);
    }
    if target.exists() {
        return Err("Plik o tej nazwie już istnieje.".to_owned());
    }
    move_no_clobber(source, &target).map_err(display_io_error)?;
    Ok(target)
}

fn rename_case_only(source: &Path, target: &Path) -> Result<PathBuf, String> {
    let temporary = reserve_rename_path(source)?;
    move_no_clobber(source, &temporary).map_err(display_io_error)?;
    match move_no_clobber(&temporary, target) {
        Ok(()) => Ok(target.to_path_buf()),
        Err(rename_error) => match move_no_clobber(&temporary, source) {
            Ok(()) => Err(format!(
                "Nie można zmienić wielkości liter w nazwie pliku: {rename_error}"
            )),
            Err(rollback_error) => Err(format!(
                "Nie można dokończyć zmiany nazwy. Plik pozostał jako {}. Błąd: {rename_error}; przywracanie: {rollback_error}",
                temporary.display()
            )),
        },
    }
}

fn reserve_rename_path(source: &Path) -> Result<PathBuf, String> {
    let parent = source
        .parent()
        .ok_or_else(|| "Nie można utworzyć nazwy tymczasowej.".to_owned())?;
    for _ in 0..100 {
        let id = NEXT_TEMP_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let path = parent.join(format!(
            ".skaner-rename-recovery-{}-{id}.pdf",
            std::process::id()
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err("Nie można utworzyć unikalnej nazwy tymczasowej.".to_owned())
}

/// Moves every PDF independently. Name collisions are retained as `name (2).pdf`, etc.
pub fn move_pdfs_keep_both(sources: &[PathBuf], target_folder: &Path) -> FileActionReport {
    let mut report = FileActionReport::default();
    if !target_folder.is_dir() {
        let error = "Folder docelowy nie istnieje.".to_owned();
        for source in sources {
            report.failure(source.clone(), error.clone());
        }
        return report;
    }

    for source in sources {
        if same_directory(source.parent(), target_folder) {
            report.skip(source.clone());
            continue;
        }
        match move_pdf_keep_both(source, target_folder) {
            Ok(destination) => report.success(source.clone(), Some(destination)),
            Err(error) => report.failure(source.clone(), error),
        }
    }
    report
}

fn move_pdf_keep_both(source: &Path, target_folder: &Path) -> Result<PathBuf, String> {
    validate_pdf_source(source)?;
    let requested_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "Nazwa pliku nie jest obsługiwana.".to_owned())?;

    for _ in 0..100 {
        let target = unique_pdf_path(target_folder, requested_name)?;
        match move_no_clobber(source, &target) {
            Ok(()) => return Ok(target),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists || target.exists() => {
                continue;
            }
            Err(error) => return Err(display_io_error(error)),
        }
    }
    Err("Nie można utworzyć unikalnej nazwy pliku.".to_owned())
}

fn same_directory(source_parent: Option<&Path>, target: &Path) -> bool {
    let Some(source_parent) = source_parent else {
        return false;
    };
    match (fs::canonicalize(source_parent), fs::canonicalize(target)) {
        (Ok(source), Ok(target)) => source == target,
        _ => source_parent == target,
    }
}

/// Sends files to the operating system recycle bin. It never falls back to permanent deletion.
pub fn recycle_pdfs(sources: &[PathBuf]) -> FileActionReport {
    let mut report = FileActionReport::default();
    for source in sources {
        if let Err(error) = validate_pdf_source(source) {
            report.failure(source.clone(), error);
            continue;
        }
        match recycle_pdf(source) {
            Ok(()) => report.success(source.clone(), None),
            Err(error) => report.failure(source.clone(), error),
        }
    }
    report
}

#[cfg(not(windows))]
fn recycle_pdf(source: &Path) -> Result<(), String> {
    trash::delete(source).map_err(|error| format!("Nie można przenieść pliku do Kosza: {error}"))
}

#[cfg(windows)]
fn recycle_pdf(source: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    };
    use windows::Win32::UI::Shell::{
        FOF_NO_UI, FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation,
        IShellItem, SHCreateItemFromParsingName,
    };
    use windows::core::PCWSTR;

    let file_name = source
        .file_name()
        .ok_or_else(|| "Nie można ustalić nazwy pliku dla Kosza.".to_owned())?;
    let parent = source
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // Resolve the directory but deliberately keep the final path component
    // unresolved. This guarantees that the selected directory entry, not a
    // possible symlink target outside the library, is sent to the Recycle Bin.
    let full_path = fs::canonicalize(parent)
        .map_err(display_io_error)?
        .join(file_name);
    let wide_path: Vec<u16> = full_path.as_os_str().encode_wide().chain(Some(0)).collect();
    let extended_prefix = ['\\' as u16, '\\' as u16, '?' as u16, '\\' as u16];
    let shell_path = wide_path
        .strip_prefix(&extended_prefix)
        .unwrap_or(&wide_path);
    unsafe {
        // The UI thread may already be initialized in a different COM apartment.
        // In that case CoCreateInstance can use the existing apartment.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let operation: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)
            .map_err(|error| format!("Nie można otworzyć systemowego Kosza: {error}"))?;
        operation
            .SetOperationFlags(FOF_NO_UI | FOFX_EARLYFAILURE | FOFX_RECYCLEONDELETE)
            .map_err(|error| format!("Nie można przygotować operacji Kosza: {error}"))?;
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(shell_path.as_ptr()), None)
            .map_err(|error| format!("Nie można odczytać pliku dla Kosza: {error}"))?;
        operation
            .DeleteItem(&item, None)
            .map_err(|error| format!("Nie można dodać pliku do operacji Kosza: {error}"))?;
        operation
            .PerformOperations()
            .map_err(|error| format!("Nie można przenieść pliku do Kosza: {error}"))?;
        if operation
            .GetAnyOperationsAborted()
            .map_err(|error| format!("Nie można potwierdzić operacji Kosza: {error}"))?
            .as_bool()
        {
            return Err("System przerwał przenoszenie pliku do Kosza.".to_owned());
        }
    }
    Ok(())
}

fn validate_pdf_source(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Plik nie istnieje.".to_owned()
        } else {
            display_io_error(error)
        }
    })?;
    if metadata.file_type().is_symlink() {
        return Err("Skróty i dowiązania do plików PDF nie są obsługiwane.".to_owned());
    }
    if !metadata.is_file() {
        return Err("Plik nie istnieje.".to_owned());
    }
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        return Err("Wybrany plik nie jest plikiem PDF.".to_owned());
    }
    Ok(())
}

fn move_no_clobber(source: &Path, target: &Path) -> std::io::Result<()> {
    crate::atomic_file::move_new(source, target)
}

enum RecoveryArtifact {
    /// `X.pdf.part-<pid>-<n>` — an interrupted atomic write; pure garbage.
    PartFile,
    /// `.X.pdf.skaner-edit.lock` — a lock sidecar left by a crashed process.
    StaleLock,
    /// `.X.replace-recovery-*.pdf` — the pre-save version preserved when a
    /// crash hit between ReplaceFileW and cleanup; superseded but recoverable.
    ReplaceBackup,
    /// `.X.scan-save-conflict-*.pdf` — OUR version that lost a save race;
    /// genuinely valuable user data that must become visible.
    Conflict,
    /// `.skaner-rename-recovery-*.pdf` — a document stranded mid-rename.
    RenameLeftover,
}

fn classify_recovery_artifact(name: &str) -> Option<RecoveryArtifact> {
    if !name.starts_with('.') {
        return name
            .contains(".part-")
            .then_some(RecoveryArtifact::PartFile);
    }
    if name.ends_with(".skaner-edit.lock") {
        return Some(RecoveryArtifact::StaleLock);
    }
    if name.to_lowercase().ends_with(".pdf") {
        if name.contains(".scan-save-conflict-") {
            return Some(RecoveryArtifact::Conflict);
        }
        if name.contains(".replace-recovery-") {
            return Some(RecoveryArtifact::ReplaceBackup);
        }
        if name.starts_with(".skaner-rename-recovery-") {
            return Some(RecoveryArtifact::RenameLeftover);
        }
    }
    None
}

/// Startup housekeeping over the library root and its direct child folders.
///
/// Crash leftovers from the atomic save machinery otherwise accumulate
/// forever (and, before dot-files were filtered from the index, even showed
/// up as documents). Valuable artifacts are surfaced, not deleted: conflict
/// and rename leftovers become visible PDFs, replace-backups go to the
/// recycle bin, and only `.part` fragments and stale locks are removed —
/// each of those only after `min_age`, so an operation that is literally in
/// flight is never touched.
pub fn sweep_recovery_artifacts(root: &Path, min_age: std::time::Duration) -> FileActionReport {
    let mut report = FileActionReport::default();
    let Ok(entries) = fs::read_dir(root) else {
        return report;
    };
    let mut folders = vec![root.to_path_buf()];
    for entry in entries.filter_map(Result::ok) {
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && !entry.file_name().to_string_lossy().starts_with('.')
        {
            folders.push(entry.path());
        }
    }
    for folder in folders {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(artifact) = classify_recovery_artifact(name) else {
                continue;
            };
            let path = entry.path();
            let old_enough = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= min_age);
            match artifact {
                RecoveryArtifact::PartFile | RecoveryArtifact::StaleLock => {
                    if old_enough {
                        match fs::remove_file(&path) {
                            Ok(()) => report.success(path, None),
                            Err(error) => report.failure(path, error.to_string()),
                        }
                    }
                }
                RecoveryArtifact::ReplaceBackup => {
                    if old_enough {
                        match recycle_pdf(&path) {
                            Ok(()) => report.success(path, None),
                            Err(error) => report.failure(path, error),
                        }
                    }
                }
                RecoveryArtifact::Conflict | RecoveryArtifact::RenameLeftover => {
                    let stem = match artifact {
                        RecoveryArtifact::Conflict => name
                            .strip_prefix('.')
                            .and_then(|rest| rest.split(".scan-save-conflict-").next())
                            .filter(|stem| !stem.is_empty())
                            .map(|stem| format!("{stem} (konflikt zapisu)"))
                            .unwrap_or_else(|| "Odzyskany dokument (konflikt zapisu)".to_owned()),
                        _ => "Odzyskany dokument".to_owned(),
                    };
                    let target = match unique_pdf_path(&folder, &stem) {
                        Ok(target) => target,
                        Err(error) => {
                            report.failure(path, error);
                            continue;
                        }
                    };
                    match move_no_clobber(&path, &target) {
                        Ok(()) => report.success(path, Some(target)),
                        Err(error) => report.failure(path, error.to_string()),
                    }
                }
            }
        }
    }
    report
}

fn display_io_error(error: std::io::Error) -> String {
    format!("Błąd systemu plików: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let id = NEXT_TEST_DIR.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "skaner-library-{label}-{}-{nonce}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pdf(name: &str, folder: &str, modified_secs: u64, size_bytes: u64) -> PdfInfo {
        let folder_path = PathBuf::from(folder);
        PdfInfo {
            name: name.to_owned(),
            path: folder_path.join(name),
            folder_name: folder.to_owned(),
            folder_path,
            size_bytes,
            modified_secs,
        }
    }

    #[test]
    fn natural_order_places_9_before_10() {
        let mut names = ["A10.pdf", "A2.pdf", "A09.pdf", "A9.pdf"];
        names.sort_by(|left, right| natural_cmp(left, right));
        assert_eq!(names, ["A2.pdf", "A9.pdf", "A09.pdf", "A10.pdf"]);
    }

    #[test]
    fn search_ignores_case_and_polish_diacritics_and_can_scope_folder() {
        let first = pdf("ZAŻÓŁĆ 10.pdf", "Łódź", 10, 100);
        let second = pdf("Inny.pdf", "Warszawa", 20, 200);
        let snapshot = LibrarySnapshot {
            root: PathBuf::from("library"),
            folders: Vec::new(),
            pdfs: vec![first.clone(), second],
        };

        let matches = search_and_sort(&snapshot, "zazolc lodz", SortMode::Name, true, None);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].path, first.path);
        assert!(
            search_and_sort(
                &snapshot,
                "zazolc",
                SortMode::Name,
                true,
                Some(Path::new("Warszawa"))
            )
            .is_empty()
        );
    }

    #[test]
    fn all_sort_modes_work_in_both_directions() {
        let mut items = vec![
            pdf("A10.pdf", "B", 10, 500),
            pdf("A2.pdf", "A", 30, 100),
            pdf("A1.pdf", "C", 20, 900),
        ];
        sort_pdfs(&mut items, SortMode::Modified, false);
        assert_eq!(items[0].name, "A2.pdf");
        sort_pdfs(&mut items, SortMode::Modified, true);
        assert_eq!(items[0].name, "A10.pdf");
        sort_pdfs(&mut items, SortMode::Name, true);
        assert_eq!(items[0].name, "A1.pdf");
        sort_pdfs(&mut items, SortMode::Name, false);
        assert_eq!(items[0].name, "A10.pdf");
        sort_pdfs(&mut items, SortMode::Folder, true);
        assert_eq!(items[0].folder_name, "A");
        sort_pdfs(&mut items, SortMode::Folder, false);
        assert_eq!(items[0].folder_name, "C");
        sort_pdfs(&mut items, SortMode::Size, false);
        assert_eq!(items[0].size_bytes, 900);
        sort_pdfs(&mut items, SortMode::Size, true);
        assert_eq!(items[0].size_bytes, 100);
    }

    #[test]
    fn default_directions_match_user_expectations() {
        assert!(SortMode::Name.default_ascending());
        assert!(SortMode::Folder.default_ascending());
        assert!(!SortMode::Modified.default_ascending());
        assert!(!SortMode::Size.default_ascending());
    }

    #[test]
    fn searches_and_naturally_sorts_ten_thousand_metadata_rows() {
        let snapshot = LibrarySnapshot {
            root: PathBuf::from("library"),
            folders: Vec::new(),
            pdfs: (0..10_000)
                .rev()
                .map(|number| pdf(&format!("A{number:05}.pdf"), "Akta", number, 100))
                .collect(),
        };
        let started = Instant::now();

        let matches = search_and_sort(&snapshot, "akta", SortMode::Name, true, None);

        assert_eq!(matches.len(), 10_000);
        assert_eq!(matches.first().unwrap().name, "A00000.pdf");
        assert_eq!(matches.last().unwrap().name, "A09999.pdf");
        let elapsed = started.elapsed();
        eprintln!("10k metadata search/sort: {elapsed:?}");
        assert!(
            elapsed < Duration::from_secs(5),
            "10k metadata search/sort is too slow: {:?}",
            elapsed
        );
    }

    #[test]
    fn scan_indexes_malformed_pdf_without_parsing_contents() {
        let root = TestDir::new("scan");
        let folder = root.path().join("Klient 10");
        fs::create_dir(&folder).expect("create folder");
        fs::write(folder.join("A10.pdf"), b"not actually a pdf").expect("write file");
        fs::write(root.path().join("root.pdf"), b"also not a pdf").expect("write root file");
        fs::write(folder.join("ignored.txt"), b"ignored").expect("write other file");

        let snapshot = scan_library(root.path()).expect("scan library");
        assert_eq!(snapshot.folders.len(), 1);
        assert_eq!(snapshot.folders[0].pdf_count, 1);
        assert_eq!(snapshot.pdfs.len(), 2);
        assert!(snapshot.pdfs.iter().any(|item| item.name == "A10.pdf"));
        assert!(snapshot.pdfs.iter().any(|item| item.name == "root.pdf"));
    }

    #[test]
    fn latest_refresh_generation_wins() {
        let old_root = TestDir::new("old-generation");
        let new_root = TestDir::new("new-generation");
        fs::write(old_root.path().join("old.pdf"), b"old").expect("write old file");
        fs::write(new_root.path().join("new.pdf"), b"new").expect("write new file");

        let mut worker = LibraryWorker::start();
        worker
            .request_refresh(old_root.path())
            .expect("request old refresh");
        let newest_generation = worker
            .request_refresh(new_root.path())
            .expect("request new refresh");
        let deadline = Instant::now() + Duration::from_secs(2);
        let event = loop {
            if let Some(event) = worker.try_recv_latest() {
                break event;
            }
            assert!(Instant::now() < deadline, "library worker timed out");
            thread::sleep(Duration::from_millis(2));
        };

        assert_eq!(event.generation(), newest_generation);
        let LibraryEvent::Refreshed { snapshot, .. } = event else {
            panic!("new refresh unexpectedly failed");
        };
        assert_eq!(snapshot.root, new_root.path());
        assert_eq!(snapshot.pdfs.len(), 1);
        assert_eq!(snapshot.pdfs[0].name, "new.pdf");
    }

    #[test]
    fn file_operations_worker_reports_completed_move() {
        let root = TestDir::new("file-worker");
        let source_folder = root.path().join("source");
        let target_folder = root.path().join("target");
        fs::create_dir(&source_folder).unwrap();
        fs::create_dir(&target_folder).unwrap();
        let source = source_folder.join("A01.pdf");
        fs::write(&source, b"pdf").unwrap();
        let worker = FileOperationWorker::start();
        worker
            .request_move(vec![source], target_folder.clone())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let event = loop {
            if let Some(event) = worker.try_recv().unwrap() {
                break event;
            }
            assert!(Instant::now() < deadline, "file worker timed out");
            thread::sleep(Duration::from_millis(2));
        };
        let FileOperationEvent::Moved(report) = event else {
            panic!("unexpected file-operation event");
        };
        assert_eq!(report.completed_count(), 1);
        assert!(target_folder.join("A01.pdf").is_file());
    }

    #[test]
    fn sweep_surfaces_conflicts_and_removes_fragments() {
        let root = TestDir::new("sweep");
        let folder = root.path().join("Akta");
        fs::create_dir(&folder).expect("create folder");
        fs::write(folder.join("Umowa.pdf"), b"dokument").expect("document");
        fs::write(folder.join("Umowa.pdf.part-999-1"), b"czesc").expect("part");
        fs::write(folder.join(".Umowa.scan-save-conflict-999-2.pdf"), b"konflikt")
            .expect("conflict");
        fs::write(
            root.path().join(".skaner-rename-recovery-999-3.pdf"),
            b"po-rename",
        )
        .expect("rename leftover");
        fs::write(folder.join(".Umowa.pdf.skaner-edit.lock"), b"").expect("stale lock");

        let report = sweep_recovery_artifacts(root.path(), std::time::Duration::ZERO);

        assert!(!folder.join("Umowa.pdf.part-999-1").exists());
        assert!(!folder.join(".Umowa.scan-save-conflict-999-2.pdf").exists());
        assert_eq!(
            fs::read(folder.join("Umowa (konflikt zapisu).pdf")).expect("surfaced conflict"),
            b"konflikt"
        );
        assert_eq!(
            fs::read(root.path().join("Odzyskany dokument.pdf")).expect("surfaced leftover"),
            b"po-rename"
        );
        assert!(!folder.join(".Umowa.pdf.skaner-edit.lock").exists());
        assert!(folder.join("Umowa.pdf").exists(), "zwykły dokument nietknięty");
        assert!(report.failures.is_empty(), "{:?}", report.failures);
    }

    #[test]
    fn sweep_never_deletes_fresh_in_flight_files() {
        let root = TestDir::new("sweep-age");
        fs::write(root.path().join("Doc.pdf.part-1-1"), b"czesc").expect("part");
        sweep_recovery_artifacts(root.path(), std::time::Duration::from_secs(3600));
        assert!(root.path().join("Doc.pdf.part-1-1").exists());
    }

    #[test]
    fn rename_and_move_preserve_bytes_and_keep_collisions() {
        let root = TestDir::new("operations");
        let source_folder = root.path().join("source");
        let target_folder = root.path().join("target");
        fs::create_dir(&source_folder).expect("create source folder");
        fs::create_dir(&target_folder).expect("create target folder");
        let original = source_folder.join("Report.pdf");
        let bytes = b"unchanged pdf bytes";
        fs::write(&original, bytes).expect("write source");

        let renamed = rename_pdf(&original, "REPORT.PDF").expect("rename file");
        assert_eq!(fs::read(&renamed).expect("read renamed"), bytes);
        fs::write(target_folder.join("REPORT.pdf"), b"existing").expect("write collision");

        let report = move_pdfs_keep_both(std::slice::from_ref(&renamed), &target_folder);
        assert!(report.all_succeeded());
        let destination = report.successes[0]
            .destination
            .as_ref()
            .expect("move destination");
        assert_eq!(destination.file_name().unwrap(), "REPORT (2).pdf");
        assert_eq!(fs::read(destination).expect("read moved"), bytes);
        assert!(!renamed.exists());
    }

    #[test]
    fn batch_reports_each_failure_without_permanent_delete_fallback() {
        let root = TestDir::new("reports");
        let missing = root.path().join("missing.pdf");
        let move_report = move_pdfs_keep_both(
            std::slice::from_ref(&missing),
            &root.path().join("missing-folder"),
        );
        assert_eq!(move_report.completed_count(), 0);
        assert_eq!(move_report.failed_count(), 1);

        let recycle_report = recycle_pdfs(&[missing]);
        assert_eq!(recycle_report.completed_count(), 0);
        assert_eq!(recycle_report.failed_count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn strict_windows_recycle_can_restore_the_same_pdf() {
        let root = TestDir::new("strict-recycle");
        let source = root.path().join(format!(
            "recycle-{}-{}.pdf",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, AtomicOrdering::Relaxed)
        ));
        fs::write(&source, b"recoverable pdf bytes").expect("write recyclable PDF");

        let report = recycle_pdfs(std::slice::from_ref(&source));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!source.exists());

        // The recycle itself is verified above. The restore round-trip uses
        // the `trash` crate, whose Recycle Bin listing misses items on some
        // machines even though Explorer shows them (observed 2026-08-05 on
        // the production laptop: shell Namespace(10) listed the file, this
        // call did not). Run the round-trip only where the listing works.
        let item = trash::os_limited::list()
            .expect("list Recycle Bin")
            .into_iter()
            .find(|item| item.original_path() == source);
        let Some(item) = item else {
            eprintln!(
                "strict-recycle: trash listing missed the item on this machine; restore round-trip skipped"
            );
            return;
        };
        trash::os_limited::restore_all([item]).expect("restore recycled PDF");
        assert_eq!(fs::read(&source).unwrap(), b"recoverable pdf bytes");
    }
}
