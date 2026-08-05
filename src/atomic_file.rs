use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

pub struct PreparedFile {
    part_path: Option<PathBuf>,
}

/// A process-wide edit lock represented by an exclusively opened sidecar file.
///
/// The sidecar is needed because holding the PDF itself open without delete
/// sharing would also prevent `ReplaceFileW` from atomically replacing it.
pub struct UpdateLock {
    file: Option<File>,
    path: PathBuf,
    remove_on_drop: bool,
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

pub fn content_fingerprint(path: &Path) -> io::Result<u64> {
    let bytes = fs::read(path)?;
    Ok(fingerprint_bytes(&bytes))
}

fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[cfg(windows)]
fn locked_content_fingerprint_windows(path: &Path) -> io::Result<(File, u64)> {
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    let mut file = OpenOptions::new()
        .read(true)
        // Deliberately omit FILE_SHARE_WRITE. An existing or new writer must
        // not be able to mutate the exact backup while it is being verified.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
        .open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((file, fingerprint_bytes(&bytes)))
}

/// Moves an existing file or directory only when `target` does not exist.
///
/// Unlike `std::fs::rename` on Windows, this never replaces a racing target.
pub fn move_new(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        move_new_windows(source, target)
    }

    #[cfg(not(windows))]
    {
        fs::hard_link(source, target)?;
        if let Err(error) = fs::remove_file(source) {
            let _ = fs::remove_file(target);
            return Err(error);
        }
        Ok(())
    }
}

/// Prevents two scanner processes from updating the same PDF concurrently.
pub fn try_lock_for_update(target: &Path) -> io::Result<UpdateLock> {
    let path = update_lock_path(target)?;

    #[cfg(windows)]
    {
        match open_windows_update_lock(&path, true) {
            Ok(file) => Ok(UpdateLock {
                file: Some(file),
                path,
                remove_on_drop: true,
            }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                open_windows_update_lock(&path, false).map(|file| UpdateLock {
                    file: Some(file),
                    path,
                    // The existing sidecar may not have been created by this
                    // process, so never delete it on the fallback path.
                    remove_on_drop: false,
                })
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(not(windows))]
    {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(UpdateLock {
            file: Some(file),
            path,
            remove_on_drop: true,
        })
    }
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
        match commit_replace(part_path, target) {
            Ok(()) => {
                self.part_path = None;
                Ok(())
            }
            Err(error) if !target.exists() => {
                let preserved = self.part_path.take().expect("prepared file has path");
                Err(io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; plik zastępczy zachowano jako {}",
                        preserved.display()
                    ),
                ))
            }
            Err(error) => Err(error),
        }
    }

    /// Atomically updates a file only if the exact version replaced on disk
    /// matches `expected_fingerprint`.
    ///
    /// On Windows, `ReplaceFileW` first preserves the exact replaced file as a
    /// backup. We validate that backup before deleting it and restore it if an
    /// external editor raced the save.
    pub fn commit_replace_if_unchanged(
        self,
        target: &Path,
        expected_fingerprint: u64,
    ) -> io::Result<()> {
        #[cfg(windows)]
        {
            let mut prepared = self;
            let part_path = prepared
                .part_path
                .as_deref()
                .expect("prepared file has path");
            match replace_existing_windows_if_unchanged(part_path, target, expected_fingerprint) {
                Ok(()) => {
                    prepared.part_path = None;
                    Ok(())
                }
                Err(failure) => {
                    if failure.replacement_consumed {
                        prepared.part_path = None;
                    } else if !target.exists() {
                        let preserved = prepared.part_path.take().expect("prepared file has path");
                        return Err(io::Error::new(
                            failure.error.kind(),
                            format!(
                                "{}; plik zastępczy zachowano jako {}",
                                failure.error,
                                preserved.display()
                            ),
                        ));
                    }
                    Err(failure.error)
                }
            }
        }

        #[cfg(not(windows))]
        {
            if content_fingerprint(target)? != expected_fingerprint {
                return Err(io::Error::other("source file changed before replacement"));
            }
            self.commit_replace(target)
        }
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
    move_new(replacement, target)
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        drop(self.file.take());
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn update_lock_path(target: &Path) -> io::Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?;
    let file_name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no file name"))?;
    let mut lock_name = OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".skaner-edit.lock");
    Ok(parent.join(lock_name))
}

#[cfg(windows)]
fn open_windows_update_lock(path: &Path, create_new: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .share_mode(0)
        .attributes(FILE_ATTRIBUTE_HIDDEN);
    if create_new {
        options.create_new(true);
    } else {
        // The original owner may remove the sidecar between our two open
        // attempts. Creating it here keeps the lock acquisition atomic.
        options.create(true);
    }
    options.open(path)
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
    let backup_path = unique_recovery_path(target)?;
    replace_file_with_backup_windows(replacement, target, &backup_path)?;
    let _ = fs::remove_file(backup_path);
    Ok(())
}

#[cfg(windows)]
struct CheckedReplaceFailure {
    error: io::Error,
    replacement_consumed: bool,
}

#[cfg(windows)]
fn replace_existing_windows_if_unchanged(
    replacement: &Path,
    target: &Path,
    expected_fingerprint: u64,
) -> Result<(), CheckedReplaceFailure> {
    if !target.exists() {
        return Err(CheckedReplaceFailure {
            error: io::Error::new(io::ErrorKind::NotFound, "source file no longer exists"),
            replacement_consumed: false,
        });
    }

    let backup_path = unique_recovery_path(target).map_err(|error| CheckedReplaceFailure {
        error,
        replacement_consumed: false,
    })?;
    if let Err(error) = replace_file_with_backup_windows(replacement, target, &backup_path) {
        return Err(CheckedReplaceFailure {
            error,
            replacement_consumed: !replacement.exists(),
        });
    }

    let (backup_guard, backup_fingerprint) = match locked_content_fingerprint_windows(&backup_path)
    {
        Ok(result) => result,
        Err(error) => {
            return Err(CheckedReplaceFailure {
                error: io::Error::new(
                    error.kind(),
                    format!(
                        "Nie można bezpiecznie sprawdzić wersji zastąpionej podczas zapisu: {error}. Nie usunięto żadnej wersji: nowy plik jest w {}, a wersja zewnętrzna w {}.",
                        target.display(),
                        backup_path.display()
                    ),
                ),
                replacement_consumed: true,
            });
        }
    };
    if backup_fingerprint == expected_fingerprint {
        // Keep the no-write-sharing handle alive until the backup has been
        // marked for deletion. This closes the last writer race between hash
        // verification and cleanup.
        let cleanup = fs::remove_file(&backup_path);
        drop(backup_guard);
        return cleanup.map_err(|error| CheckedReplaceFailure {
            error: io::Error::new(
                error.kind(),
                format!(
                    "Zapisano nową wersję w {}, ale nie można usunąć zweryfikowanej kopii bezpieczeństwa {}: {error}. Obie wersje zachowano.",
                    target.display(),
                    backup_path.display()
                ),
            ),
            replacement_consumed: true,
        });
    }
    drop(backup_guard);

    let conflict_path = match unique_conflict_path(target) {
        Ok(path) => path,
        Err(error) => {
            return Err(CheckedReplaceFailure {
                error: io::Error::new(
                    error.kind(),
                    format!(
                        "Plik źródłowy zmienił się podczas zapisu. Nie usunięto żadnej wersji: nowy plik jest w {}, a wersja zewnętrzna w {}. Nie można przygotować ścieżki przywracania: {error}",
                        target.display(),
                        backup_path.display()
                    ),
                ),
                replacement_consumed: true,
            });
        }
    };

    match replace_file_with_backup_windows(&backup_path, target, &conflict_path) {
        Ok(()) => Err(CheckedReplaceFailure {
            error: io::Error::other(format!(
                "Plik źródłowy zmienił się podczas zapisu. Przywrócono wersję zewnętrzną, a wersję przygotowaną przez Skaner zachowano jako {}.",
                conflict_path.display()
            )),
            replacement_consumed: true,
        }),
        Err(rollback_error) => Err(CheckedReplaceFailure {
            error: io::Error::other(format!(
                "Plik źródłowy zmienił się podczas zapisu. Automatyczne przywrócenie nie powiodło się: {rollback_error}. Nie usunięto zachowanych wersji; sprawdź {}, {} oraz {}.",
                target.display(),
                backup_path.display(),
                conflict_path.display()
            )),
            replacement_consumed: true,
        }),
    }
}

#[cfg(windows)]
fn replace_file_with_backup_windows(
    replacement: &Path,
    target: &Path,
    backup_path: &Path,
) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let replacement_wide: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let backup_wide: Vec<u16> = backup_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let replaced = unsafe {
        replace_file_w(
            target_wide.as_ptr(),
            replacement_wide.as_ptr(),
            backup_wide.as_ptr(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if replaced == 0 {
        let replace_error = io::Error::last_os_error();
        if !target.exists()
            && backup_path.exists()
            && let Err(restore_error) = move_new_windows(backup_path, target)
        {
            return Err(io::Error::new(
                replace_error.kind(),
                format!(
                    "{replace_error}; nie udało się przywrócić oryginału z {}: {restore_error}",
                    backup_path.display()
                ),
            ));
        }
        Err(replace_error)
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn unique_recovery_path(target: &Path) -> io::Result<PathBuf> {
    unique_sibling_path(target, "replace-recovery")
}

#[cfg(windows)]
fn unique_conflict_path(target: &Path) -> io::Result<PathBuf> {
    unique_sibling_path(target, "scan-save-conflict")
}

#[cfg(windows)]
fn unique_sibling_path(target: &Path, label: &str) -> io::Result<PathBuf> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_stem()
        .unwrap_or(target.as_os_str())
        .to_string_lossy();
    let extension = target
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    for _ in 0..100 {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            ".{stem}.{label}-{}-{id}{extension}",
            std::process::id()
        ));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "nie można utworzyć unikalnej nazwy kopii bezpieczeństwa",
    ))
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
        #[cfg(windows)]
        {
            let parent = path.parent().expect("target parent");
            let recovery_prefix = format!(
                ".{}.replace-recovery-",
                path.file_stem().unwrap().to_string_lossy()
            );
            assert!(
                fs::read_dir(parent)
                    .expect("read target parent")
                    .filter_map(Result::ok)
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&recovery_prefix)),
                "successful replacement must not leave a recovery copy"
            );
        }
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

    #[test]
    fn move_new_never_replaces_an_existing_target() {
        let source = test_path("move-new-source");
        let target = test_path("move-new-target");
        fs::write(&source, b"ours").expect("seed source");
        fs::write(&target, b"theirs").expect("seed target");

        assert!(move_new(&source, &target).is_err());
        assert_eq!(fs::read(&source).expect("read source"), b"ours");
        assert_eq!(fs::read(&target).expect("read target"), b"theirs");

        fs::remove_file(source).expect("cleanup source");
        fs::remove_file(target).expect("cleanup target");
    }

    #[cfg(windows)]
    #[test]
    fn update_lock_excludes_a_second_process_handle() {
        let target = test_path("update-lock");
        fs::write(&target, b"pdf").expect("seed target");

        let first = try_lock_for_update(&target).expect("acquire first lock");
        assert!(
            try_lock_for_update(&target).is_err(),
            "a concurrent update lock must be rejected"
        );
        drop(first);
        let second = try_lock_for_update(&target).expect("reacquire released lock");
        drop(second);

        let _ = fs::remove_file(update_lock_path(&target).unwrap());
        fs::remove_file(target).expect("cleanup target");
    }

    #[cfg(windows)]
    #[test]
    fn checked_replace_commits_when_the_exact_target_is_unchanged() {
        let target = test_path("checked-success");
        fs::write(&target, b"old").expect("seed target");
        let expected = content_fingerprint(&target).expect("fingerprint original");
        let prepared = prepare(&target, b"new").expect("prepare update");

        prepared
            .commit_replace_if_unchanged(&target, expected)
            .expect("commit checked update");

        assert_eq!(fs::read(&target).expect("read target"), b"new");
        fs::remove_file(target).expect("cleanup target");
    }

    #[cfg(windows)]
    #[test]
    fn checked_replace_restores_an_external_change_and_preserves_our_version() {
        let target = test_path("checked-conflict");
        fs::write(&target, b"version opened by scanner").expect("seed target");
        let expected = content_fingerprint(&target).expect("fingerprint original");
        let prepared = prepare(&target, b"version prepared by scanner").expect("prepare update");
        fs::write(&target, b"version written by another editor").expect("external update");

        let error = prepared
            .commit_replace_if_unchanged(&target, expected)
            .expect_err("external update must win");

        assert!(error.to_string().contains("zmienił się podczas zapisu"));
        assert_eq!(
            fs::read(&target).expect("read restored target"),
            b"version written by another editor"
        );
        let parent = target.parent().expect("target parent");
        let conflict_prefix = format!(
            ".{}.scan-save-conflict-",
            target.file_stem().unwrap().to_string_lossy()
        );
        let conflict = fs::read_dir(parent)
            .expect("read target parent")
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&conflict_prefix)
            })
            .expect("scanner version must be preserved")
            .path();
        assert_eq!(
            fs::read(&conflict).expect("read scanner conflict copy"),
            b"version prepared by scanner"
        );

        fs::remove_file(conflict).expect("cleanup conflict copy");
        fs::remove_file(target).expect("cleanup target");
    }

    #[cfg(windows)]
    #[test]
    fn checked_replace_never_deletes_a_backup_with_an_active_writer() {
        use std::io::{Seek, SeekFrom};
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;
        let target = test_path("checked-active-writer");
        fs::write(&target, b"version opened by scanner").expect("seed target");
        let expected = content_fingerprint(&target).expect("fingerprint original");
        let mut external_writer = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&target)
            .expect("open sharing writer");
        let prepared = prepare(&target, b"version prepared by scanner").expect("prepare update");

        let error = prepared
            .commit_replace_if_unchanged(&target, expected)
            .expect_err("an actively writable backup must never be deleted");
        assert!(error.to_string().contains("bezpiecznie sprawdzić"));

        external_writer
            .set_len(0)
            .expect("truncate external version");
        external_writer
            .seek(SeekFrom::Start(0))
            .expect("rewind external version");
        external_writer
            .write_all(b"version completed by active external writer")
            .expect("finish external write");
        external_writer.sync_all().expect("sync external write");
        drop(external_writer);

        assert_eq!(
            fs::read(&target).expect("read scanner target"),
            b"version prepared by scanner"
        );
        let parent = target.parent().expect("target parent");
        let recovery_prefix = format!(
            ".{}.replace-recovery-",
            target.file_stem().unwrap().to_string_lossy()
        );
        let backup = fs::read_dir(parent)
            .expect("read target parent")
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&recovery_prefix)
            })
            .expect("actively written backup must remain")
            .path();
        assert_eq!(
            fs::read(&backup).expect("read external backup"),
            b"version completed by active external writer"
        );

        fs::remove_file(backup).expect("cleanup external backup");
        fs::remove_file(target).expect("cleanup scanner target");
    }
}
