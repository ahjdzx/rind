//! 移入废纸篓、从废纸篓放回。只用 `NSFileManager.trashItem(at:resultingItemURL:)`，
//! 这样能拿到废纸篓里的实际路径（重名时 macOS 会改名）。

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2_foundation::{NSError, NSFileManager, NSString, NSURL};

#[derive(Debug, thiserror::Error)]
pub enum TrashError {
    #[error("not found: {0}")]
    NotFound(PathBuf),
    #[error("permission denied: {0}")]
    PermissionDenied(PathBuf),
    #[error("destination exists: {0}")]
    DestinationExists(PathBuf),
    #[error("{0}")]
    Other(String),
}

/// 移入废纸篓，返回它在废纸篓里的路径。只收绝对路径：相对路径会按当前目录解析，不是调用方想的那个。
pub fn move_to_trash(path: &Path) -> Result<PathBuf, TrashError> {
    if !path.is_absolute() {
        return Err(TrashError::Other(format!(
            "{}: not an absolute path",
            path.display()
        )));
    }
    // 不跟符号链接：移动的是链接本身。
    let meta = fs::symlink_metadata(path).map_err(|e| io_error(e, path))?;

    let ns_path = NSString::from_str(&path.to_string_lossy());
    let url = NSURL::fileURLWithPath_isDirectory(&ns_path, meta.is_dir());
    let mut resulting: Option<Retained<NSURL>> = None;
    let manager = NSFileManager::defaultManager();
    match manager.trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting)) {
        Ok(()) => resulting
            .and_then(|url| url.path())
            .map(|p| PathBuf::from(p.to_string()))
            .ok_or_else(|| {
                TrashError::Other(format!(
                    "moved {} but macOS did not say where",
                    path.display()
                ))
            }),
        Err(err) => Err(ns_error(&err, path)),
    }
}

/// 从废纸篓放回原位。父目录不在就建；原位置已被占用 → `DestinationExists`，而且是由 `renamex_np(RENAME_EXCL)`
/// 在内核里保证的，没有"先看再动"的空窗；跨卷时先复制、核对过文件数和大小再删源，复制出错两边都留着。
pub fn restore(trashed: &Path, original: &Path) -> Result<(), TrashError> {
    if fs::symlink_metadata(trashed).is_err() {
        return Err(TrashError::NotFound(trashed.to_path_buf()));
    }
    if !original.is_absolute() {
        return Err(TrashError::Other(format!(
            "{}: not an absolute path",
            original.display()
        )));
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent).map_err(|e| io_error(e, parent))?;
    }
    match rename_exclusive(trashed, original) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {
            Err(TrashError::DestinationExists(original.to_path_buf()))
        }
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            if fs::symlink_metadata(original).is_ok() {
                return Err(TrashError::DestinationExists(original.to_path_buf()));
            }
            copy_recursively(trashed, original)?;
            let (from_files, from_bytes) = tally(trashed).map_err(|e| io_error(e, trashed))?;
            let (to_files, to_bytes) = tally(original).map_err(|e| io_error(e, original))?;
            if from_files != to_files || from_bytes != to_bytes {
                return Err(TrashError::Other(format!(
                    "{}: copied across volumes but the copy does not match ({to_files} files / {to_bytes} bytes vs \
                     {from_files} / {from_bytes}); both copies were left in place",
                    original.display()
                )));
            }
            remove_recursively(trashed).map_err(|e| io_error(e, trashed))
        }
        Err(e) => Err(io_error(e, trashed)),
    }
}

/// `rename(2)` 会覆盖已有的目标；`renamex_np` 带 `RENAME_EXCL` 时目标存在就报 `EEXIST`。
fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("path contains NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("path contains NUL"))?;
    // SAFETY: 两个指针都来自活着的 CString，调用期间不会被释放。
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn copy_recursively(from: &Path, to: &Path) -> Result<(), TrashError> {
    let meta = fs::symlink_metadata(from).map_err(|e| io_error(e, from))?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(from).map_err(|e| io_error(e, from))?;
        std::os::unix::fs::symlink(&target, to).map_err(|e| io_error(e, to))?;
    } else if file_type.is_dir() {
        fs::create_dir(to).map_err(|e| io_error(e, to))?;
        for entry in fs::read_dir(from).map_err(|e| io_error(e, from))? {
            let entry = entry.map_err(|e| io_error(e, from))?;
            copy_recursively(&entry.path(), &to.join(entry.file_name()))?;
        }
        fs::set_permissions(to, meta.permissions()).map_err(|e| io_error(e, to))?;
    } else {
        // 目标已经有东西（比如有人趁机放了个符号链接）就不写：`create_new` 不跟链接、不覆盖。
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)
            .map_err(|e| io_error(e, to))?;
        let mut source = fs::File::open(from).map_err(|e| io_error(e, from))?;
        io::copy(&mut source, &mut target).map_err(|e| io_error(e, from))?;
        fs::set_permissions(to, meta.permissions()).map_err(|e| io_error(e, to))?;
    }
    Ok(())
}

/// 一棵树里有多少个条目、多少字节（不跟符号链接，链接本身算一个条目、零字节）。
fn tally(path: &Path) -> io::Result<(u64, u64)> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok((1, 0));
    }
    if !meta.is_dir() {
        return Ok((1, meta.len()));
    }
    let mut files = 1;
    let mut bytes = 0;
    for entry in fs::read_dir(path)? {
        let (f, b) = tally(&entry?.path())?;
        files += f;
        bytes += b;
    }
    Ok((files, bytes))
}

fn remove_recursively(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn io_error(error: io::Error, path: &Path) -> TrashError {
    match error.kind() {
        io::ErrorKind::NotFound => TrashError::NotFound(path.to_path_buf()),
        io::ErrorKind::PermissionDenied => TrashError::PermissionDenied(path.to_path_buf()),
        io::ErrorKind::AlreadyExists => TrashError::DestinationExists(path.to_path_buf()),
        _ => TrashError::Other(format!("{}: {error}", path.display())),
    }
}

/// `NSCocoaErrorDomain` 的 4 / 260 是不存在，257 / 513 是没权限；`NSPOSIXErrorDomain` 用 errno。
fn ns_error(error: &NSError, path: &Path) -> TrashError {
    let domain = error.domain().to_string();
    let code = error.code();
    match (domain.as_str(), code) {
        ("NSCocoaErrorDomain", 4 | 260) | ("NSPOSIXErrorDomain", 2) => {
            TrashError::NotFound(path.to_path_buf())
        }
        ("NSCocoaErrorDomain", 257 | 513) | ("NSPOSIXErrorDomain", 1 | 13) => {
            TrashError::PermissionDenied(path.to_path_buf())
        }
        _ => TrashError::Other(format!(
            "{}: {} ({domain} {code})",
            path.display(),
            error.localizedDescription()
        )),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// 废纸篓在用户的家目录下，所以要走同卷 rename 的测试文件也得在那儿。
    pub fn scratch() -> tempfile::TempDir {
        let base = dirs::home_dir().unwrap().join("Library/Caches/rind-tests");
        fs::create_dir_all(&base).unwrap();
        tempfile::tempdir_in(base).unwrap()
    }

    /// 测试里进了废纸篓的东西，哪怕断言 panic 了也放回来，不给用户的废纸篓留东西。
    pub struct PutBack {
        pub items: Vec<(PathBuf, PathBuf)>,
    }

    impl PutBack {
        pub fn new() -> Self {
            PutBack { items: Vec::new() }
        }

        pub fn trash(&mut self, path: &Path) -> Result<PathBuf, TrashError> {
            let trashed = move_to_trash(path)?;
            self.items.push((trashed.clone(), path.to_path_buf()));
            Ok(trashed)
        }
    }

    impl Drop for PutBack {
        fn drop(&mut self) {
            for (trashed, original) in self.items.drain(..) {
                if fs::symlink_metadata(&trashed).is_ok() {
                    let _ = fs::remove_dir_all(&original).or_else(|_| fs::remove_file(&original));
                    let _ = restore(&trashed, &original);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{scratch, PutBack};
    use super::*;

    #[test]
    fn file_goes_to_trash_and_comes_back() {
        let dir = scratch();
        let mut guard = PutBack::new();
        let file = dir.path().join("rind-trash-test.txt");
        fs::write(&file, b"hello").unwrap();

        let trashed = guard.trash(&file).expect("move to trash");
        assert!(!file.exists(), "the original is gone");
        assert!(
            trashed.exists(),
            "it is in the Trash at {}",
            trashed.display()
        );
        assert!(
            trashed.to_string_lossy().contains(".Trash"),
            "{}",
            trashed.display()
        );

        restore(&trashed, &file).expect("restore");
        assert_eq!(fs::read(&file).unwrap(), b"hello");
        assert!(!trashed.exists());
    }

    #[test]
    fn folder_goes_to_trash_and_comes_back_with_contents() {
        let dir = scratch();
        let mut guard = PutBack::new();
        let folder = dir.path().join("rind-folder");
        fs::create_dir_all(folder.join("inner")).unwrap();
        fs::write(folder.join("inner/a.txt"), b"a").unwrap();

        let trashed = guard.trash(&folder).unwrap();
        assert!(!folder.exists());
        assert!(trashed.join("inner/a.txt").exists());

        // 原位置的父目录没了也能放回。
        fs::remove_dir_all(dir.path()).unwrap();
        restore(&trashed, &folder).unwrap();
        assert_eq!(fs::read(folder.join("inner/a.txt")).unwrap(), b"a");
        assert!(!trashed.exists());
    }

    #[test]
    fn missing_or_relative_paths_are_refused() {
        let dir = scratch();
        let missing = dir.path().join("nope");
        assert!(matches!(move_to_trash(&missing), Err(TrashError::NotFound(p)) if p == missing));
        assert!(matches!(
            restore(&missing, &dir.path().join("x")),
            Err(TrashError::NotFound(_))
        ));
        assert!(matches!(
            move_to_trash(Path::new("relative/file")),
            Err(TrashError::Other(_))
        ));
        let file = dir.path().join("f");
        fs::write(&file, b"1").unwrap();
        assert!(matches!(
            restore(&file, Path::new("relative/target")),
            Err(TrashError::Other(_))
        ));
        assert!(file.exists(), "nothing happened to it");
    }

    #[test]
    fn restore_refuses_to_overwrite() {
        let dir = scratch();
        let mut guard = PutBack::new();
        let file = dir.path().join("f.txt");
        fs::write(&file, b"1").unwrap();
        let trashed = guard.trash(&file).unwrap();
        fs::write(&file, b"2").unwrap();

        assert!(
            matches!(restore(&trashed, &file), Err(TrashError::DestinationExists(p)) if p == file)
        );
        assert_eq!(
            fs::read(&file).unwrap(),
            b"2",
            "the newer file is untouched"
        );
        assert!(trashed.exists(), "and the trashed one is still there");

        // 收拾干净：放回另一个名字再删。
        let elsewhere = dir.path().join("f-restored.txt");
        restore(&trashed, &elsewhere).unwrap();
        assert_eq!(fs::read(&elsewhere).unwrap(), b"1");
    }

    #[test]
    fn rename_exclusive_never_replaces_a_file_that_appeared_meanwhile() {
        // 没有"先看再动"的空窗：目标存在就是 EEXIST，哪怕它是在检查之后才出现的。
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let err = rename_exclusive(&a, &b).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EEXIST));
        assert_eq!(fs::read(&b).unwrap(), b"b");
        fs::remove_file(&b).unwrap();
        rename_exclusive(&a, &b).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"a");
    }

    #[test]
    fn restore_copies_across_volumes_and_verifies() {
        // /tmp 在 macOS 上是另一个卷（或至少是另一个挂载点）；两边都不是真的废纸篓，但 restore 不关心。
        let dir = scratch();
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("tree");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("sub/x.bin"), b"xyz").unwrap();
        std::os::unix::fs::symlink("sub/x.bin", source.join("link")).unwrap();

        let target = dir.path().join("tree");
        restore(&source, &target).unwrap();
        assert_eq!(fs::read(target.join("sub/x.bin")).unwrap(), b"xyz");
        assert_eq!(
            fs::read_link(target.join("link")).unwrap(),
            PathBuf::from("sub/x.bin")
        );
        assert!(
            !source.exists(),
            "the source is removed after a verified copy"
        );
    }

    #[test]
    fn a_failed_cross_volume_copy_keeps_the_source() {
        let dir = scratch();
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("tree");
        fs::create_dir_all(source.join("sub")).unwrap();
        fs::write(source.join("sub/x.bin"), b"xyz").unwrap();
        // 目标目录已经有一个同名文件挡在半路：复制到那儿会失败。
        let target = dir.path().join("tree");
        fs::create_dir_all(target.join("sub")).unwrap();
        fs::write(target.join("sub/x.bin"), b"old").unwrap();

        let err = restore(&source, &target).unwrap_err();
        assert!(matches!(err, TrashError::DestinationExists(_)), "{err}");
        assert!(source.join("sub/x.bin").exists(), "the source is untouched");
        assert_eq!(fs::read(target.join("sub/x.bin")).unwrap(), b"old");
    }
}
