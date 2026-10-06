//! Collision-free naming. Received data never replaces anything: names are
//! claimed with atomic "fail if exists" operations, never check-then-act.

use std::io;
use std::path::{Path, PathBuf};

/// Extensions that belong together (`archive (2).tar.gz`, not `archive.tar (2).gz`).
const DOUBLE_EXTENSIONS: [&str; 5] = [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz"];

/// The `n`-th candidate for `name`: `name` itself for n = 1, then
/// `stem (2).ext`, `stem (3).ext`, …
pub fn candidate(name: &str, n: u32) -> String {
    if n <= 1 {
        return name.to_string();
    }
    let lower = name.to_lowercase();
    let split =
        DOUBLE_EXTENSIONS.iter().find(|ext| lower.ends_with(*ext) && lower.len() > ext.len()).map(|ext| name.len() - ext.len()).or_else(
            || match name.rfind('.') {
                // A leading dot is a hidden file, not an extension.
                Some(dot) if dot > 0 => Some(dot),
                _ => None,
            },
        );
    match split {
        Some(at) => format!("{} ({n}){}", &name[..at], &name[at..]),
        None => format!("{name} ({n})"),
    }
}

/// Moves `from` into `dir` under `name`, or the first free variant of it.
/// Returns the final path. Never replaces an existing file.
pub fn rename_unique(from: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    for n in 1..=10_000 {
        let target = dir.join(candidate(name, n));
        match rename_noreplace(from, &target) {
            Ok(()) => return Ok(target),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free file name"))
}

/// Creates a new folder `dir/name` (or the first free variant) and returns it.
pub fn create_unique_dir(dir: &Path, name: &str) -> io::Result<PathBuf> {
    for n in 1..=10_000 {
        let target = dir.join(candidate(name, n));
        match std::fs::create_dir(&target) {
            Ok(()) => return Ok(target),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free folder name"))
}

/// Writes a received file's `data` as a new file `dir/name` (or the first
/// free variant) in one operation, with timestamps set on the open handle.
/// Used for small files, where a part file + rename would double the
/// filesystem work. The Mark-of-the-Web goes on right after the exclusive
/// create, before any content, so the content is never visible untagged.
/// Never replaces anything; on a write error the new file is removed.
pub fn write_new_unique(
    dir: &Path,
    name: &str,
    data: &[u8],
    modified: Option<std::time::SystemTime>,
    accessed: Option<std::time::SystemTime>,
) -> io::Result<PathBuf> {
    use std::io::Write;
    for n in 1..=10_000 {
        let target = dir.join(candidate(name, n));
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };
        super::motw::mark_from_network(&target);
        let written = file.write_all(data).map(|_| {
            if modified.is_some() || accessed.is_some() {
                let mut times = std::fs::FileTimes::new();
                if let Some(m) = modified {
                    times = times.set_modified(m);
                }
                if let Some(a) = accessed {
                    times = times.set_accessed(a);
                }
                // Best effort, like for large files.
                let _ = file.set_times(times);
            }
        });
        if let Err(err) = written {
            drop(file);
            let _ = std::fs::remove_file(&target);
            return Err(err);
        }
        return Ok(target);
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free file name"))
}

/// Renames `from` to `to`, failing with `AlreadyExists` instead of replacing.
pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    imp::rename_noreplace(from, to)
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        // Long paths need the verbatim prefix; it requires an absolute path.
        let absolute = std::path::absolute(path)?;
        let mut s: Vec<u16> = absolute.as_os_str().encode_wide().collect();
        if s.len() >= 240 && !absolute.as_os_str().to_string_lossy().starts_with(r"\\?\") {
            let mut prefixed: Vec<u16> = r"\\?\".encode_utf16().collect();
            prefixed.append(&mut s);
            s = prefixed;
        }
        s.push(0);
        Ok(s)
    }

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let from = wide(from)?;
        let to = wide(to)?;
        // Without MOVEFILE_REPLACE_EXISTING the call fails if `to` exists.
        let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
        if ok != 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // ERROR_FILE_EXISTS, ERROR_ALREADY_EXISTS
            Some(80) | Some(183) => Err(io::Error::new(io::ErrorKind::AlreadyExists, err)),
            _ => Err(err),
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos", target_os = "ios"))))]
mod imp {
    use std::io;
    use std::path::Path;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        super::link_then_unlink(from, to)
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let f = CString::new(from.as_os_str().as_bytes())?;
        let t = CString::new(to.as_os_str().as_bytes())?;
        let rc = unsafe { libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // Filesystem or kernel without RENAME_NOREPLACE support.
            Some(libc::EINVAL) | Some(libc::ENOSYS) => super::link_then_unlink(from, to),
            _ => Err(err),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
        let f = CString::new(from.as_os_str().as_bytes())?;
        let t = CString::new(to.as_os_str().as_bytes())?;
        let rc = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EINVAL) => super::link_then_unlink(from, to),
            _ => Err(err),
        }
    }
}

/// Portable no-replace rename: creating a hard link fails if the target
/// exists. Filesystems without hard links (exFAT/FAT SD cards) fall back to a
/// check-then-rename, the only option left there.
#[cfg(unix)]
fn link_then_unlink(from: &Path, to: &Path) -> io::Result<()> {
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(err),
        Err(_) => {
            if to.symlink_metadata().is_ok() {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "target exists"));
            }
            std::fs::rename(from, to)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_number_before_the_extension() {
        assert_eq!(candidate("photo.jpg", 1), "photo.jpg");
        assert_eq!(candidate("photo.jpg", 2), "photo (2).jpg");
        assert_eq!(candidate("archive.tar.gz", 3), "archive (3).tar.gz");
        assert_eq!(candidate("README", 2), "README (2)");
        assert_eq!(candidate(".bashrc", 2), ".bashrc (2)");
    }

    #[test]
    fn rename_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "existing").unwrap();
        std::fs::write(dir.path().join("a (2).txt"), "existing 2").unwrap();
        let part = dir.path().join("incoming.ferrypart");
        std::fs::write(&part, "new").unwrap();

        let final_path = rename_unique(&part, dir.path(), "a.txt").unwrap();
        assert_eq!(final_path.file_name().unwrap(), "a (3).txt");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "existing");
        assert_eq!(std::fs::read_to_string(&final_path).unwrap(), "new");
        assert!(!part.exists());
    }

    #[test]
    fn noreplace_reports_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, "a").unwrap();
        std::fs::write(&b, "b").unwrap();
        let err = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "b");
    }

    #[test]
    fn small_writes_never_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old").unwrap();
        let p = write_new_unique(dir.path(), "a.txt", b"new", None, None).unwrap();
        assert_eq!(p.file_name().unwrap(), "a (2).txt");
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(p).unwrap(), "new");
    }

    #[test]
    fn unique_dirs_do_not_merge_transfers() {
        let dir = tempfile::tempdir().unwrap();
        let first = create_unique_dir(dir.path(), "Album").unwrap();
        let second = create_unique_dir(dir.path(), "Album").unwrap();
        assert_eq!(first.file_name().unwrap(), "Album");
        assert_eq!(second.file_name().unwrap(), "Album (2)");
    }
}
