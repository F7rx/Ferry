//! Free disk space, checked before accepting a transfer and before each file.

use std::io;
use std::path::Path;

/// Bytes available to this user on the volume holding `path` (an existing directory).
pub fn available_space(path: &Path) -> io::Result<u64> {
    imp::available_space(path)
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    pub fn available_space(path: &Path) -> io::Result<u64> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut available: u64 = 0;
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, std::ptr::null_mut(), std::ptr::null_mut()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(available)
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn available_space(path: &Path) -> io::Result<u64> {
        let c = CString::new(path.as_os_str().as_bytes())?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        #[allow(clippy::unnecessary_cast)]
        Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reports_some_space_for_temp_dir() {
        let space = super::available_space(&std::env::temp_dir()).unwrap();
        assert!(space > 0);
    }
}
