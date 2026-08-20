use crate::document::{CropPoint, EncodedPage, PageEncoding};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct Manifest {
    folder_path: PathBuf,
    started_at: u64,
    page_ids: Vec<u64>,
    #[serde(default)]
    page_revisions: BTreeMap<u64, u64>,
}

#[derive(Debug, PartialEq)]
pub struct RecoveredPage {
    pub id: u64,
    /// `width/height` are 0 for format-0/1 sessions (JPEG only); the app
    /// reads them from the JPEG header in that case.
    pub page: Option<EncodedPage>,
    pub original_jpeg: Option<Vec<u8>>,
    pub corners: Option<[CropPoint; 4]>,
    pub quarter_turns: u8,
}

pub struct RecoveredSession {
    pub folder_path: PathBuf,
    pub pages: Vec<RecoveredPage>,
    pub skipped_pages: usize,
    /// Highest page id the session ever recorded — including pages whose
    /// files were lost. The next capture must start above it so ids are
    /// never reused against a stale manifest position.
    pub highest_page_id: u64,
}

enum ManifestState {
    Missing,
    Parsed(Manifest),
    /// The file exists but cannot be read or parsed. Page files may still be
    /// intact, so this state must never lead to a silent wipe.
    Corrupt,
}

/// Format 0 (legacy) stored the page JPEG already rotated, so its
/// `quarter_turns` must not be applied again at display time. Format 1 keeps
/// the JPEG unrotated and treats `quarter_turns` as display metadata. Format 2
/// adds the page `encoding` (the page file is `.g4` for G4) and its pixel
/// dimensions, without which a G4 stream cannot be decoded.
#[derive(Serialize, Deserialize)]
struct PageMetadata {
    corners: [CropPoint; 4],
    #[serde(default)]
    quarter_turns: u8,
    #[serde(default)]
    format: u8,
    #[serde(default = "default_encoding")]
    encoding: PageEncoding,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

fn default_encoding() -> PageEncoding {
    PageEncoding::Jpeg
}

const PAGE_METADATA_FORMAT: u8 = 2;

fn page_extension(encoding: PageEncoding) -> &'static str {
    match encoding {
        PageEncoding::Jpeg => "jpg",
        PageEncoding::G4 => "g4",
    }
}

pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    pub fn open_default() -> Option<Self> {
        ProjectDirs::from("pl", "SkanerDokumentow", "Skaner dokumentów").map(|dirs| Self {
            dir: dirs.data_dir().join("sesja"),
        })
    }

    #[cfg(test)]
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.ron")
    }

    fn page_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.jpg"))
    }

    fn original_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.original.jpg"))
    }

    fn metadata_path(&self, id: u64) -> PathBuf {
        self.dir.join(format!("{id}.crop.ron"))
    }

    fn revision_paths(&self, id: u64, revision: u64) -> (PathBuf, PathBuf, PathBuf) {
        (
            self.dir.join(format!("{id}.r{revision}.jpg")),
            self.dir.join(format!("{id}.r{revision}.original.jpg")),
            self.dir.join(format!("{id}.r{revision}.crop.ron")),
        )
    }

    fn paths_for(&self, id: u64, revision: Option<u64>) -> (PathBuf, PathBuf, PathBuf) {
        revision.map_or_else(
            || {
                (
                    self.page_path(id),
                    self.original_path(id),
                    self.metadata_path(id),
                )
            },
            |revision| self.revision_paths(id, revision),
        )
    }

    /// The page file for an encoding: `.jpg` for JPEG, `.g4` for G4 — same stem.
    fn page_path_for(&self, id: u64, revision: Option<u64>, encoding: PageEncoding) -> PathBuf {
        self.paths_for(id, revision)
            .0
            .with_extension(page_extension(encoding))
    }

    /// Reads whichever page file exists (JPEG first, then G4) and tells which.
    fn read_page_file(&self, id: u64, revision: Option<u64>) -> Option<(Vec<u8>, PageEncoding)> {
        for encoding in [PageEncoding::Jpeg, PageEncoding::G4] {
            if let Ok(bytes) = fs::read(self.page_path_for(id, revision, encoding)) {
                return Some((bytes, encoding));
            }
        }
        None
    }

    /// Combines a page file with its metadata. A JPEG without format-2
    /// metadata gets 0×0 dimensions (the app reads them from the header); a
    /// G4 stream without trustworthy dimensions is unusable and yields `None`.
    fn recovered_encoded_page(
        page_file: Option<(Vec<u8>, PageEncoding)>,
        metadata: Option<&PageMetadata>,
    ) -> Option<EncodedPage> {
        let (bytes, file_encoding) = page_file?;
        let modern = metadata.filter(|metadata| metadata.format >= 2);
        match file_encoding {
            PageEncoding::Jpeg => Some(EncodedPage {
                bytes,
                encoding: PageEncoding::Jpeg,
                width: modern.map_or(0, |metadata| metadata.width),
                height: modern.map_or(0, |metadata| metadata.height),
            }),
            PageEncoding::G4 => modern
                .filter(|metadata| {
                    metadata.encoding == PageEncoding::G4 && metadata.width > 0 && metadata.height > 0
                })
                .map(|metadata| EncodedPage {
                    bytes,
                    encoding: PageEncoding::G4,
                    width: metadata.width,
                    height: metadata.height,
                }),
        }
    }

    fn remove_revision_files(&self, id: u64, revision: Option<u64>) {
        let (page, original, metadata) = self.paths_for(id, revision);
        let _ = fs::remove_file(page.with_extension("g4"));
        let _ = fs::remove_file(page);
        let _ = fs::remove_file(original);
        let _ = fs::remove_file(metadata);
    }

    fn manifest_state(&self) -> ManifestState {
        match fs::read_to_string(self.manifest_path()) {
            Ok(contents) => match ron::from_str(&contents) {
                Ok(manifest) => ManifestState::Parsed(manifest),
                Err(_) => ManifestState::Corrupt,
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ManifestState::Missing,
            Err(_) => ManifestState::Corrupt,
        }
    }

    fn read_manifest(&self) -> Option<Manifest> {
        match self.manifest_state() {
            ManifestState::Parsed(manifest) => Some(manifest),
            ManifestState::Missing | ManifestState::Corrupt => None,
        }
    }

    fn write_manifest(&self, manifest: &Manifest) -> Result<(), String> {
        let contents = ron::to_string(manifest).map_err(|error| error.to_string())?;
        crate::atomic_file::write(&self.manifest_path(), contents).map_err(io_error)
    }

    pub fn begin(&self, folder: &Path) -> Result<(), String> {
        self.clear()?;
        fs::create_dir_all(&self.dir).map_err(io_error)?;
        self.write_manifest(&Manifest {
            folder_path: folder.to_path_buf(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
            page_ids: Vec::new(),
            page_revisions: BTreeMap::new(),
        })
    }

    pub fn write_page(
        &self,
        id: u64,
        page: &EncodedPage,
        original_jpeg: &[u8],
        corners: [CropPoint; 4],
        quarter_turns: u8,
    ) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        let previous_revision = manifest.page_revisions.get(&id).copied();
        let revision = previous_revision.unwrap_or(0) + 1;
        let (_, original_path, metadata_path) = self.revision_paths(id, revision);
        let page_path = self.page_path_for(id, Some(revision), page.encoding);
        crate::atomic_file::write(&page_path, &page.bytes).map_err(io_error)?;
        crate::atomic_file::write(&original_path, original_jpeg).map_err(io_error)?;
        let metadata = ron::to_string(&PageMetadata {
            corners,
            quarter_turns: quarter_turns % 4,
            format: PAGE_METADATA_FORMAT,
            encoding: page.encoding,
            width: page.width,
            height: page.height,
        })
        .map_err(|error| error.to_string())?;
        crate::atomic_file::write(&metadata_path, metadata).map_err(io_error)?;
        if !manifest.page_ids.contains(&id) {
            manifest.page_ids.push(id);
        }
        manifest.page_revisions.insert(id, revision);
        self.write_manifest(&manifest)?;
        self.remove_revision_files(id, previous_revision);
        Ok(())
    }

    pub fn remove_page(&self, id: u64) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        let revision = manifest.page_revisions.remove(&id);
        manifest.page_ids.retain(|existing| *existing != id);
        self.write_manifest(&manifest)?;
        self.remove_revision_files(id, revision);
        if revision.is_some() {
            self.remove_revision_files(id, None);
        }
        Ok(())
    }

    pub fn set_order(&self, ids: &[u64]) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        manifest.page_ids = ids.to_vec();
        self.write_manifest(&manifest)
    }

    pub fn load_existing(&self) -> Option<RecoveredSession> {
        match self.manifest_state() {
            ManifestState::Parsed(manifest) => self.load_from_manifest(manifest),
            ManifestState::Missing => None,
            ManifestState::Corrupt => self.salvage_orphan_pages(),
        }
    }

    fn load_from_manifest(&self, manifest: Manifest) -> Option<RecoveredSession> {
        if manifest.page_ids.is_empty() {
            return None;
        }
        let mut pages = Vec::new();
        let mut skipped_pages = 0;
        for id in &manifest.page_ids {
            let revision = manifest.page_revisions.get(id).copied();
            let (_, original_path, metadata_path) = self.paths_for(*id, revision);
            let page_file = self.read_page_file(*id, revision);
            let original_jpeg = fs::read(original_path).ok();
            if page_file.is_none() && original_jpeg.is_none() {
                skipped_pages += 1;
                continue;
            }
            let metadata = fs::read_to_string(metadata_path)
                .ok()
                .and_then(|contents| ron::from_str::<PageMetadata>(&contents).ok());
            pages.push(RecoveredPage {
                id: *id,
                page: Self::recovered_encoded_page(page_file, metadata.as_ref()),
                original_jpeg,
                corners: metadata.as_ref().map(|metadata| metadata.corners),
                quarter_turns: metadata
                    .as_ref()
                    // Legacy (format 0) JPEGs are already rotated on disk.
                    .filter(|metadata| metadata.format >= 1)
                    .map(|metadata| metadata.quarter_turns % 4)
                    .unwrap_or(0),
            });
        }
        let highest_page_id = manifest.page_ids.iter().copied().max().unwrap_or(0);
        Some(RecoveredSession {
            folder_path: manifest.folder_path,
            pages,
            skipped_pages,
            highest_page_id,
        })
    }

    /// Recovers page files next to an unreadable manifest instead of letting
    /// the next `begin()` wipe them, and writes a fresh manifest so the
    /// restored session can keep persisting.
    fn salvage_orphan_pages(&self) -> Option<RecoveredSession> {
        let mut revisions_by_id: BTreeMap<u64, Vec<Option<u64>>> = BTreeMap::new();
        for entry in fs::read_dir(&self.dir).ok()?.filter_map(Result::ok) {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some((id, revision, kind)) = parse_session_file_name(name)
                && matches!(kind, SessionFileKind::Page | SessionFileKind::Original)
            {
                revisions_by_id.entry(id).or_default().push(revision);
            }
        }

        let mut pages = Vec::new();
        let mut manifest = Manifest {
            folder_path: PathBuf::new(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0),
            page_ids: Vec::new(),
            page_revisions: BTreeMap::new(),
        };
        for (id, mut revisions) in revisions_by_id {
            revisions.sort_unstable();
            let Some(revision) = revisions.pop() else {
                continue;
            };
            let (_, original_path, metadata_path) = self.paths_for(id, revision);
            let page_file = self.read_page_file(id, revision);
            let original_jpeg = fs::read(original_path).ok();
            if page_file.is_none() && original_jpeg.is_none() {
                continue;
            }
            let metadata = fs::read_to_string(metadata_path)
                .ok()
                .and_then(|contents| ron::from_str::<PageMetadata>(&contents).ok());
            manifest.page_ids.push(id);
            if let Some(revision) = revision {
                manifest.page_revisions.insert(id, revision);
            }
            pages.push(RecoveredPage {
                id,
                page: Self::recovered_encoded_page(page_file, metadata.as_ref()),
                original_jpeg,
                corners: metadata.as_ref().map(|metadata| metadata.corners),
                quarter_turns: metadata
                    .as_ref()
                    // Legacy (format 0) JPEGs are already rotated on disk.
                    .filter(|metadata| metadata.format >= 1)
                    .map(|metadata| metadata.quarter_turns % 4)
                    .unwrap_or(0),
            });
        }
        if pages.is_empty() {
            return None;
        }
        let highest_page_id = manifest.page_ids.iter().copied().max().unwrap_or(0);
        // Best effort: a failed rewrite only means later page writes will
        // surface the same error through the session-broken toast.
        let _ = self.write_manifest(&manifest);
        Some(RecoveredSession {
            folder_path: PathBuf::new(),
            pages,
            skipped_pages: 0,
            highest_page_id,
        })
    }

    /// Points the persisted session at a different destination folder, e.g.
    /// when a recovered session is restored into the library root because its
    /// original folder no longer exists.
    pub fn set_folder(&self, folder: &Path) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        manifest.folder_path = folder.to_path_buf();
        self.write_manifest(&manifest)
    }

    pub fn clear(&self) -> Result<(), String> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir).map_err(io_error)?;
        }
        Ok(())
    }
}

enum SessionCommand {
    Begin { folder: PathBuf },
    WritePage {
        id: u64,
        page: EncodedPage,
        original_jpeg: Vec<u8>,
        corners: [CropPoint; 4],
        quarter_turns: u8,
    },
    RemovePage { id: u64 },
    SetOrder { ids: Vec<u64> },
    SetFolder { folder: PathBuf },
    Clear,
}

/// Runs every session mutation on its own thread, in submission order.
///
/// Each page write is four fsync-ed atomic replacements — on a slow or
/// AV-scanned disk that stalled the UI for up to a second per page when done
/// inline. Ordering (and therefore crash consistency) is unchanged because
/// the worker is strictly sequential, and dropping the worker joins it only
/// after the queue has fully drained, so exiting the app still flushes the
/// session.
pub struct SessionWorker {
    command_tx: Option<Sender<SessionCommand>>,
    error_rx: Receiver<String>,
    thread: Option<JoinHandle<()>>,
}

impl SessionWorker {
    pub fn start(store: SessionStore) -> Self {
        let (command_tx, command_rx) = channel::<SessionCommand>();
        let (error_tx, error_rx) = channel();
        let thread = std::thread::Builder::new()
            .name("session-persist".to_owned())
            .spawn(move || {
                while let Ok(command) = command_rx.recv() {
                    let result = match command {
                        SessionCommand::Begin { folder } => store.begin(&folder),
                        SessionCommand::WritePage {
                            id,
                            page,
                            original_jpeg,
                            corners,
                            quarter_turns,
                        } => store.write_page(id, &page, &original_jpeg, corners, quarter_turns),
                        SessionCommand::RemovePage { id } => store.remove_page(id),
                        SessionCommand::SetOrder { ids } => store.set_order(&ids),
                        SessionCommand::SetFolder { folder } => store.set_folder(&folder),
                        SessionCommand::Clear => store.clear(),
                    };
                    if let Err(error) = result
                        && error_tx.send(error).is_err()
                    {
                        return;
                    }
                }
            })
            .expect("cannot start session persist thread");
        Self {
            command_tx: Some(command_tx),
            error_rx,
            thread: Some(thread),
        }
    }

    fn send(&self, command: SessionCommand) {
        if let Some(command_tx) = &self.command_tx {
            let _ = command_tx.send(command);
        }
    }

    pub fn begin(&self, folder: &Path) {
        self.send(SessionCommand::Begin {
            folder: folder.to_path_buf(),
        });
    }

    pub fn write_page(
        &self,
        id: u64,
        page: &EncodedPage,
        original_jpeg: &[u8],
        corners: [CropPoint; 4],
        quarter_turns: u8,
    ) {
        self.send(SessionCommand::WritePage {
            id,
            page: page.clone(),
            original_jpeg: original_jpeg.to_vec(),
            corners,
            quarter_turns,
        });
    }

    pub fn remove_page(&self, id: u64) {
        self.send(SessionCommand::RemovePage { id });
    }

    pub fn set_order(&self, ids: Vec<u64>) {
        self.send(SessionCommand::SetOrder { ids });
    }

    pub fn set_folder(&self, folder: &Path) {
        self.send(SessionCommand::SetFolder {
            folder: folder.to_path_buf(),
        });
    }

    pub fn clear(&self) {
        self.send(SessionCommand::Clear);
    }

    /// First persistence error, if any occurred since the last poll.
    pub fn try_recv_error(&self) -> Option<String> {
        match self.error_rx.try_recv() {
            Ok(error) => Some(error),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                Some("Wątek zapisu sesji nieoczekiwanie zakończył pracę.".to_owned())
            }
        }
    }
}

impl Drop for SessionWorker {
    fn drop(&mut self) {
        drop(self.command_tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum SessionFileKind {
    Page,
    Original,
    Metadata,
}

fn parse_session_file_name(name: &str) -> Option<(u64, Option<u64>, SessionFileKind)> {
    let (head, kind) = if let Some(head) = name.strip_suffix(".original.jpg") {
        (head, SessionFileKind::Original)
    } else if let Some(head) = name.strip_suffix(".crop.ron") {
        (head, SessionFileKind::Metadata)
    } else if let Some(head) = name.strip_suffix(".g4") {
        (head, SessionFileKind::Page)
    } else {
        (name.strip_suffix(".jpg")?, SessionFileKind::Page)
    };
    let (id_part, revision) = match head.split_once(".r") {
        Some((id_part, revision)) => (id_part, Some(revision.parse::<u64>().ok()?)),
        None => (head, None),
    };
    Some((id_part.parse::<u64>().ok()?, revision, kind))
}

fn io_error(error: std::io::Error) -> String {
    format!("Błąd zapisu sesji: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store(name: &str) -> SessionStore {
        let dir =
            std::env::temp_dir().join(format!("skaner-sesja-test-{}-{name}", std::process::id()));
        let store = SessionStore::at(dir);
        let _ = store.clear();
        store
    }

    fn jpeg(bytes: &[u8]) -> EncodedPage {
        EncodedPage {
            bytes: bytes.to_vec(),
            encoding: PageEncoding::Jpeg,
            width: 80,
            height: 120,
        }
    }

    fn g4(bytes: &[u8]) -> EncodedPage {
        EncodedPage {
            bytes: bytes.to_vec(),
            encoding: PageEncoding::G4,
            width: 2480,
            height: 3508,
        }
    }

    fn corners() -> [CropPoint; 4] {
        [
            CropPoint::new(0.1, 0.2),
            CropPoint::new(0.9, 0.2),
            CropPoint::new(0.9, 0.8),
            CropPoint::new(0.1, 0.8),
        ]
    }

    #[test]
    fn empty_session_loads_as_none() {
        let store = test_store("empty");
        store
            .begin(Path::new("D:/dokumenty/faktury"))
            .expect("begin");
        assert!(store.load_existing().is_none());
        store.clear().expect("clear");
    }

    #[test]
    fn roundtrip_preserves_pages_order_and_folder() {
        let store = test_store("roundtrip");
        let folder = Path::new("D:/dokumenty/umowy");
        store.begin(folder).expect("begin");
        store
            .write_page(5, &jpeg(b"piata-strona"), b"oryginal-5", corners(), 1)
            .expect("write 5");
        store
            .write_page(9, &jpeg(b"dziewiata-strona"), b"oryginal-9", corners(), 0)
            .expect("write 9");
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.folder_path, folder);
        assert_eq!(recovered.pages[0].id, 5);
        assert_eq!(
            recovered.pages[0].page.as_ref().map(|page| page.bytes.as_slice()),
            Some(b"piata-strona".as_slice())
        );
        assert_eq!(
            recovered.pages[0].original_jpeg.as_deref(),
            Some(b"oryginal-5".as_slice())
        );
        assert_eq!(recovered.pages[0].corners, Some(corners()));
        assert_eq!(recovered.pages[0].quarter_turns, 1);
        assert_eq!(recovered.pages[1].id, 9);
        store.clear().expect("clear");
        assert!(store.load_existing().is_none());
    }

    #[test]
    fn remove_and_reorder_update_manifest() {
        let store = test_store("mutations");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store
            .write_page(1, &jpeg(b"a"), b"oa", corners(), 0)
            .expect("write 1");
        store
            .write_page(2, &jpeg(b"b"), b"ob", corners(), 0)
            .expect("write 2");
        store
            .write_page(3, &jpeg(b"c"), b"oc", corners(), 0)
            .expect("write 3");
        store.remove_page(2).expect("remove");
        store.set_order(&[3, 1]).expect("reorder");
        let recovered = store.load_existing().expect("session");
        let ids: Vec<u64> = recovered.pages.iter().map(|page| page.id).collect();
        assert_eq!(ids, vec![3, 1]);
        store.clear().expect("clear");
    }

    #[test]
    fn write_without_manifest_errors_without_panic() {
        let store = test_store("nomanifest");
        assert!(store.write_page(1, &jpeg(b"x"), b"ox", corners(), 0).is_err());
        let _ = store.clear();
    }

    #[test]
    fn rewriting_same_id_keeps_single_entry() {
        let store = test_store("rewrite");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store
            .write_page(4, &jpeg(b"stara"), b"oryginal", corners(), 0)
            .expect("write");
        store
            .write_page(4, &jpeg(b"nowa"), b"oryginal", corners(), 0)
            .expect("rewrite");
        let manifest = store.read_manifest().expect("manifest");
        assert_eq!(manifest.page_revisions.get(&4), Some(&2));
        assert!(!store.revision_paths(4, 1).0.exists());
        assert!(store.revision_paths(4, 2).0.exists());
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages.len(), 1);
        assert_eq!(recovered.pages[0].id, 4);
        assert_eq!(recovered.pages[0].page.as_ref().map(|page| page.bytes.as_slice()), Some(b"nowa".as_slice()));
        store.clear().expect("clear");
    }

    #[test]
    fn legacy_page_without_original_still_loads() {
        let store = test_store("legacy");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        fs::write(store.page_path(7), b"stary-format").expect("legacy page");
        let mut manifest = store.read_manifest().expect("manifest");
        manifest.page_ids.push(7);
        store.write_manifest(&manifest).expect("manifest update");

        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages.len(), 1);
        assert_eq!(
            recovered.pages[0].page.as_ref().map(|page| page.bytes.as_slice()),
            Some(b"stary-format".as_slice())
        );
        assert_eq!(recovered.pages[0].original_jpeg, None);
        assert_eq!(recovered.pages[0].corners, None);
        assert_eq!(recovered.pages[0].quarter_turns, 0);
        store.clear().expect("clear");
    }

    #[test]
    fn legacy_format_zero_metadata_never_reapplies_rotation() {
        let store = test_store("legacy-format");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        fs::write(store.page_path(4), b"obrocony-jpeg").expect("legacy page");
        // Legacy metadata (no `format` field): the stored JPEG already
        // contains the rotation.
        fs::write(
            store.metadata_path(4),
            "(corners:((x:0.1,y:0.2),(x:0.9,y:0.2),(x:0.9,y:0.8),(x:0.1,y:0.8)),quarter_turns:3)",
        )
        .expect("legacy metadata");
        let mut manifest = store.read_manifest().expect("manifest");
        manifest.page_ids.push(4);
        store.write_manifest(&manifest).expect("manifest update");

        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages.len(), 1);
        assert_eq!(recovered.pages[0].quarter_turns, 0);
        assert!(recovered.pages[0].corners.is_some());
        store.clear().expect("clear");
    }

    #[test]
    fn corrupt_manifest_salvages_pages_instead_of_discarding() {
        let store = test_store("corrupt-manifest");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store
            .write_page(3, &jpeg(b"trzecia"), b"oryginal-3", corners(), 1)
            .expect("write 3");
        store
            .write_page(8, &jpeg(b"osma"), b"oryginal-8", corners(), 0)
            .expect("write 8");
        fs::write(store.manifest_path(), "###nie-ron###").expect("corrupt manifest");

        let recovered = store.load_existing().expect("salvage");
        assert_eq!(recovered.folder_path, PathBuf::new());
        let ids: Vec<u64> = recovered.pages.iter().map(|page| page.id).collect();
        assert_eq!(ids, vec![3, 8]);
        assert_eq!(
            recovered.pages[0].page.as_ref().map(|page| page.bytes.as_slice()),
            Some(b"trzecia".as_slice())
        );
        assert_eq!(recovered.pages[0].corners, Some(corners()));
        assert_eq!(recovered.pages[0].quarter_turns, 1);
        assert_eq!(recovered.highest_page_id, 8);
        // Salvage rebuilds the manifest so the restored session keeps persisting.
        store
            .write_page(9, &jpeg(b"dziewiata"), b"o9", corners(), 0)
            .expect("write after salvage");
        store.clear().expect("clear");
    }

    #[test]
    fn highest_page_id_includes_pages_with_lost_files() {
        let store = test_store("highest-id");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store.write_page(2, &jpeg(b"a"), b"oa", corners(), 0).expect("write 2");
        store.write_page(7, &jpeg(b"b"), b"ob", corners(), 0).expect("write 7");
        let manifest = store.read_manifest().expect("manifest");
        let revision = manifest.page_revisions.get(&7).copied();
        store.remove_revision_files(7, revision);

        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages.len(), 1);
        assert_eq!(recovered.skipped_pages, 1);
        assert_eq!(recovered.highest_page_id, 7);
        store.clear().expect("clear");
    }

    #[test]
    fn g4_page_round_trips_with_encoding_and_dimensions() {
        let store = test_store("g4");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store
            .write_page(2, &g4(b"g4-bajty"), b"oryginal", corners(), 1)
            .expect("write");
        assert!(store.revision_paths(2, 1).0.with_extension("g4").exists());
        assert!(
            !store.revision_paths(2, 1).0.exists(),
            "no .jpg for a G4 page"
        );
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages[0].page, Some(g4(b"g4-bajty")));
        assert_eq!(recovered.pages[0].quarter_turns, 1);
        store.clear().expect("clear");
    }

    #[test]
    fn format_one_metadata_still_restores_as_jpeg() {
        let store = test_store("format1");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        fs::write(store.page_path(4), b"jpeg-bajty").expect("page");
        fs::write(
            store.metadata_path(4),
            "(corners:((x:0.1,y:0.2),(x:0.9,y:0.2),(x:0.9,y:0.8),(x:0.1,y:0.8)),quarter_turns:2,format:1)",
        )
        .expect("metadata");
        let mut manifest = store.read_manifest().expect("manifest");
        manifest.page_ids.push(4);
        store.write_manifest(&manifest).expect("manifest update");
        let recovered = store.load_existing().expect("session");
        let page = recovered.pages[0].page.as_ref().expect("page");
        assert_eq!(page.encoding, PageEncoding::Jpeg);
        assert_eq!(page.bytes, b"jpeg-bajty");
        assert_eq!(
            (page.width, page.height),
            (0, 0),
            "legacy dims are unknown here; the app reads them from the JPEG"
        );
        assert_eq!(recovered.pages[0].quarter_turns, 2);
        store.clear().expect("clear");
    }

    #[test]
    fn orphan_g4_without_metadata_is_skipped_not_guessed() {
        let store = test_store("orphan-g4");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store
            .write_page(3, &g4(b"g4"), b"o3", corners(), 0)
            .expect("write 3");
        store
            .write_page(5, &jpeg(b"jpg"), b"o5", corners(), 0)
            .expect("write 5");
        let (_, _, metadata_3) = store.revision_paths(3, 1);
        fs::remove_file(metadata_3).expect("drop metadata of 3");
        fs::write(store.manifest_path(), "###nie-ron###").expect("corrupt manifest");
        let recovered = store.load_existing().expect("salvage");
        let ids: Vec<u64> = recovered.pages.iter().map(|page| page.id).collect();
        assert_eq!(ids, vec![3, 5]);
        assert!(
            recovered.pages[0].page.is_none(),
            "G4 bytes without dims are unusable"
        );
        assert_eq!(
            recovered.pages[0].original_jpeg.as_deref(),
            Some(b"o3".as_slice())
        );
        assert_eq!(recovered.pages[1].page, Some(jpeg(b"jpg")));
        store.clear().expect("clear");
    }

    #[test]
    fn missing_processed_page_keeps_recoverable_original() {
        let store = test_store("missing-processed");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        fs::write(store.original_path(11), b"oryginal").expect("original");
        let mut manifest = store.read_manifest().expect("manifest");
        manifest.page_ids.push(11);
        store.write_manifest(&manifest).expect("manifest update");

        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages.len(), 1);
        assert!(recovered.pages[0].page.is_none());
        assert_eq!(
            recovered.pages[0].original_jpeg.as_deref(),
            Some(b"oryginal".as_slice())
        );
        assert_eq!(recovered.skipped_pages, 0);
        store.clear().expect("clear");
    }
}
