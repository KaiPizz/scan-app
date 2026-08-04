use crate::document::CropPoint;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize)]
struct Manifest {
    folder_path: PathBuf,
    started_at: u64,
    page_ids: Vec<u64>,
}

#[derive(Debug, PartialEq)]
pub struct RecoveredPage {
    pub id: u64,
    pub jpeg: Vec<u8>,
    pub original_jpeg: Option<Vec<u8>>,
    pub corners: Option<[CropPoint; 4]>,
}

pub struct RecoveredSession {
    pub folder_path: PathBuf,
    pub pages: Vec<RecoveredPage>,
}

#[derive(Serialize, Deserialize)]
struct PageMetadata {
    corners: [CropPoint; 4],
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

    fn read_manifest(&self) -> Option<Manifest> {
        let contents = fs::read_to_string(self.manifest_path()).ok()?;
        ron::from_str(&contents).ok()
    }

    fn write_manifest(&self, manifest: &Manifest) -> Result<(), String> {
        let contents = ron::to_string(manifest).map_err(|error| error.to_string())?;
        fs::write(self.manifest_path(), contents).map_err(io_error)
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
        })
    }

    pub fn write_page(
        &self,
        id: u64,
        jpeg: &[u8],
        original_jpeg: &[u8],
        corners: [CropPoint; 4],
    ) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        fs::write(self.page_path(id), jpeg).map_err(io_error)?;
        fs::write(self.original_path(id), original_jpeg).map_err(io_error)?;
        let metadata =
            ron::to_string(&PageMetadata { corners }).map_err(|error| error.to_string())?;
        fs::write(self.metadata_path(id), metadata).map_err(io_error)?;
        if !manifest.page_ids.contains(&id) {
            manifest.page_ids.push(id);
        }
        self.write_manifest(&manifest)
    }

    pub fn remove_page(&self, id: u64) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        let _ = fs::remove_file(self.page_path(id));
        let _ = fs::remove_file(self.original_path(id));
        let _ = fs::remove_file(self.metadata_path(id));
        manifest.page_ids.retain(|existing| *existing != id);
        self.write_manifest(&manifest)
    }

    pub fn set_order(&self, ids: &[u64]) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        manifest.page_ids = ids.to_vec();
        self.write_manifest(&manifest)
    }

    pub fn load_existing(&self) -> Option<RecoveredSession> {
        let manifest = self.read_manifest()?;
        if manifest.page_ids.is_empty() {
            return None;
        }
        let mut pages = Vec::new();
        for id in &manifest.page_ids {
            let jpeg = fs::read(self.page_path(*id)).ok()?;
            let original_jpeg = fs::read(self.original_path(*id)).ok();
            let corners = fs::read_to_string(self.metadata_path(*id))
                .ok()
                .and_then(|contents| ron::from_str::<PageMetadata>(&contents).ok())
                .map(|metadata| metadata.corners);
            pages.push(RecoveredPage {
                id: *id,
                jpeg,
                original_jpeg,
                corners,
            });
        }
        Some(RecoveredSession {
            folder_path: manifest.folder_path,
            pages,
        })
    }

    pub fn clear(&self) -> Result<(), String> {
        if self.dir.exists() {
            fs::remove_dir_all(&self.dir).map_err(io_error)?;
        }
        Ok(())
    }
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
            .write_page(5, b"piata-strona", b"oryginal-5", corners())
            .expect("write 5");
        store
            .write_page(9, b"dziewiata-strona", b"oryginal-9", corners())
            .expect("write 9");
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.folder_path, folder);
        assert_eq!(recovered.pages[0].id, 5);
        assert_eq!(recovered.pages[0].jpeg, b"piata-strona");
        assert_eq!(
            recovered.pages[0].original_jpeg.as_deref(),
            Some(b"oryginal-5".as_slice())
        );
        assert_eq!(recovered.pages[0].corners, Some(corners()));
        assert_eq!(recovered.pages[1].id, 9);
        store.clear().expect("clear");
        assert!(store.load_existing().is_none());
    }

    #[test]
    fn remove_and_reorder_update_manifest() {
        let store = test_store("mutations");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store
            .write_page(1, b"a", b"oa", corners())
            .expect("write 1");
        store
            .write_page(2, b"b", b"ob", corners())
            .expect("write 2");
        store
            .write_page(3, b"c", b"oc", corners())
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
        assert!(store.write_page(1, b"x", b"ox", corners()).is_err());
        let _ = store.clear();
    }

    #[test]
    fn rewriting_same_id_keeps_single_entry() {
        let store = test_store("rewrite");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store
            .write_page(4, b"stara", b"oryginal", corners())
            .expect("write");
        store
            .write_page(4, b"nowa", b"oryginal", corners())
            .expect("rewrite");
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages.len(), 1);
        assert_eq!(recovered.pages[0].id, 4);
        assert_eq!(recovered.pages[0].jpeg, b"nowa");
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
        assert_eq!(recovered.pages[0].jpeg, b"stary-format");
        assert_eq!(recovered.pages[0].original_jpeg, None);
        assert_eq!(recovered.pages[0].corners, None);
        store.clear().expect("clear");
    }
}
