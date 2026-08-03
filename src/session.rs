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

pub struct RecoveredSession {
    pub folder_path: PathBuf,
    pub pages: Vec<(u64, Vec<u8>)>,
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

    pub fn write_page(&self, id: u64, jpeg: &[u8]) -> Result<(), String> {
        let Some(mut manifest) = self.read_manifest() else {
            return Err("Brak manifestu sesji.".to_owned());
        };
        fs::write(self.page_path(id), jpeg).map_err(io_error)?;
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
            let bytes = fs::read(self.page_path(*id)).ok()?;
            pages.push((*id, bytes));
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
        let dir = std::env::temp_dir().join(format!(
            "skaner-sesja-test-{}-{name}",
            std::process::id()
        ));
        let store = SessionStore::at(dir);
        let _ = store.clear();
        store
    }

    #[test]
    fn empty_session_loads_as_none() {
        let store = test_store("empty");
        store.begin(Path::new("D:/dokumenty/faktury")).expect("begin");
        assert!(store.load_existing().is_none());
        store.clear().expect("clear");
    }

    #[test]
    fn roundtrip_preserves_pages_order_and_folder() {
        let store = test_store("roundtrip");
        let folder = Path::new("D:/dokumenty/umowy");
        store.begin(folder).expect("begin");
        store.write_page(5, b"piata-strona").expect("write 5");
        store.write_page(9, b"dziewiata-strona").expect("write 9");
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.folder_path, folder);
        assert_eq!(
            recovered.pages,
            vec![
                (5, b"piata-strona".to_vec()),
                (9, b"dziewiata-strona".to_vec())
            ]
        );
        store.clear().expect("clear");
        assert!(store.load_existing().is_none());
    }

    #[test]
    fn remove_and_reorder_update_manifest() {
        let store = test_store("mutations");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store.write_page(1, b"a").expect("write 1");
        store.write_page(2, b"b").expect("write 2");
        store.write_page(3, b"c").expect("write 3");
        store.remove_page(2).expect("remove");
        store.set_order(&[3, 1]).expect("reorder");
        let recovered = store.load_existing().expect("session");
        let ids: Vec<u64> = recovered.pages.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![3, 1]);
        store.clear().expect("clear");
    }

    #[test]
    fn write_without_manifest_errors_without_panic() {
        let store = test_store("nomanifest");
        assert!(store.write_page(1, b"x").is_err());
        let _ = store.clear();
    }

    #[test]
    fn rewriting_same_id_keeps_single_entry() {
        let store = test_store("rewrite");
        store.begin(Path::new("D:/dokumenty")).expect("begin");
        store.write_page(4, b"stara").expect("write");
        store.write_page(4, b"nowa").expect("rewrite");
        let recovered = store.load_existing().expect("session");
        assert_eq!(recovered.pages, vec![(4, b"nowa".to_vec())]);
        store.clear().expect("clear");
    }
}
