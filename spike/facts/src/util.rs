//! Opens with a stated Windows share mode; Unix has none, so the argument is ignored there.
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug)]
pub enum Share {
    /// Read, write and delete: what std opens with.
    All,
    /// Read and write only.
    NoDelete,
    /// Nothing.
    Nothing,
}

#[cfg(windows)]
fn mode(share: Share) -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE};
    match share {
        Share::All => FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        Share::NoDelete => FILE_SHARE_READ | FILE_SHARE_WRITE,
        Share::Nothing => 0,
    }
}

/// Opens an existing file for reading.
pub fn open_read(path: &Path, share: Share) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    std::os::windows::fs::OpenOptionsExt::share_mode(&mut options, mode(share));
    #[cfg(unix)]
    let _ = share;
    options.open(path)
}

/// Opens a directory (Windows: with backup semantics, asking for attribute access only).
pub fn open_dir(path: &Path, share: Share) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES};
        OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(mode(share))
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
    }
    #[cfg(unix)]
    {
        let _ = share;
        File::open(path)
    }
}

pub fn touch(path: &Path) -> io::Result<()> {
    std::fs::write(path, b"x")
}

pub fn lock_text(r: Result<(), std::fs::TryLockError>) -> String {
    match r {
        Ok(()) => "locked".to_owned(),
        Err(std::fs::TryLockError::WouldBlock) => "would-block".to_owned(),
        Err(std::fs::TryLockError::Error(e)) => crate::out::err(&e),
    }
}
