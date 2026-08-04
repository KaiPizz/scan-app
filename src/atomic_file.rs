use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

pub struct PreparedFile {
    part_path: Option<PathBuf>,
}

pub fn prepare(target: &Path, contents: impl AsRef<[u8]>) -> io::Result<PreparedFile> {
    for _ in 0..100 {
        let part_path = unique_part_path(target);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        if let Err(error) = file
            .write_all(contents.as_ref())
            .and_then(|()| file.sync_all())
        {
            drop(file);
            let _ = fs::remove_file(&part_path);
            return Err(error);
        }
        return Ok(PreparedFile {
            part_path: Some(part_path),
        });
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "nie można utworzyć unikalnego pliku tymczasowego",
    ))
}

pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    prepare(path, contents)?.commit_replace(path)
}

impl PreparedFile {
    pub fn commit_new(mut self, target: &Path) -> io::Result<()> {
        let part_path = self.part_path.as_deref().expect("prepared file has path");
        commit_new(part_path, target)?;
        self.part_path = None;
        Ok(())
    }

    pub fn commit_replace(mut self, target: &Path) -> io::Result<()> {
        let part_path = self.part_path.as_deref().expect("prepared file has path");
        commit_replace(part_path, target)?;
        self.part_path = None;
        Ok(())
    }
}

impl Drop for PreparedFile {
    fn drop(&mut self) {
        if let Some(path) = self.part_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn unique_part_path(target: &Path) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let mut result = PathBuf::from(target.as_os_str());
    result
        .as_mut_os_string()
        .push(format!(".part-{}-{id}", std::process::id()));
    result
}

fn commit_new(replacement: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        move_new_windows(replacement, target)
    }

    #[cfg(not(windows))]
    {
        fs::hard_link(replacement, target)?;
        let _ = fs::remove_file(replacement);
        Ok(())
    }
}

#[cfg(windows)]
fn move_new_windows(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let moved = unsafe { move_file_w(source.as_ptr(), target.as_ptr()) };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn commit_replace(replacement: &Path, target: &Path) -> io::Result<()> {
    if !target.exists() {
        return commit_new(replacement, target);
    }

    #[cfg(windows)]
    {
        match replace_existing_windows(replacement, target) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                commit_new(replacement, target)
            }
            result => result,
        }
    }

    #[cfg(not(windows))]
    {
        fs::rename(replacement, target)
    }
}

#[cfg(windows)]
fn replace_existing_windows(replacement: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let replacement: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let replaced = unsafe {
        replace_file_w(
            target.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "MoveFileW"]
    fn move_file_w(existing_file_name: *const u16, new_file_name: *const u16) -> i32;

    #[link_name = "ReplaceFileW"]
    fn replace_file_w(
        replaced_file_name: *const u16,
        replacement_file_name: *const u16,
        backup_file_name: *const u16,
        replace_flags: u32,
        exclude: *mut core::ffi::c_void,
        reserved: *mut core::ffi::c_void,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("skaner-atomic-{name}-{}.txt", std::process::id()))
    }

    #[test]
    fn replaces_an_existing_file_without_removing_it_first() {
        let path = test_path("replace");
        fs::write(&path, b"old").expect("seed target");
        write(&path, b"new").expect("replace target");
        assert_eq!(fs::read(&path).expect("read target"), b"new");
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn create_new_never_clobbers_a_racing_file() {
        let path = test_path("new");
        let prepared = prepare(&path, b"ours").expect("prepare");
        fs::write(&path, b"theirs").expect("racing target");
        assert!(prepared.commit_new(&path).is_err());
        assert_eq!(fs::read(&path).expect("read target"), b"theirs");
        fs::remove_file(path).expect("cleanup");
    }
}
