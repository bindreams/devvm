//! [`Host::open_entry`](crate::host::Host::open_entry) on Windows: `NtCreateFile` relative to the handle of the
//! directory above, with `FILE_OPEN_REPARSE_POINT` so that a link at a name is opened as itself. Win32's
//! `FILE_FLAG_OPEN_REPARSE_POINT` protects only the last name of a path, which is why each name is opened on its own.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_GENERIC_READ, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TRAVERSE, FileAttributeTagInfo, FileIdInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::host::entry::components;
use crate::host::{Entry, is_absent};

#[cfg(test)]
#[path = "entry_tests.rs"]
mod entry_tests;

/// The bit of a reparse tag that says the point stands for another name: symlinks and junctions have it, a cloud
/// placeholder or a deduplicated file does not and reads as itself.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// The access a directory on the walk is opened with: enough to tell what it is and to open names in it, and not
/// `FILE_LIST_DIRECTORY`, which a directory the user may pass through can deny.
const SEARCH: u32 = FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

/// What a handle is, from its own attributes.
enum Kind {
    /// A symlink or a junction.
    Link,
    Directory,
    File,
}

pub(super) fn open_entry(root: &Path, rel: &str) -> io::Result<Entry> {
    let names = components(rel)?;
    // NT would read a backslash as a separator and walk several names in one open.
    if rel.contains('\\') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{}` is not a path below a directory", rel.escape_debug()),
        ));
    }
    // `name:stream` opens an alternate data stream of the file `name`, which is no file git tracks.
    if names.iter().any(|name| name.contains(':')) {
        return Ok(Entry::Absent);
    }
    let mut directory = open_directory(root)?;
    for i in 0..names.len() - 1 {
        let (next, kind) = match look(&directory, &names[..=i], SEARCH) {
            Ok(found) => found,
            Err(e) if is_absent(&e) => return Ok(Entry::Absent),
            Err(e) => return Err(e),
        };
        match kind {
            Kind::Directory => directory = next,
            Kind::Link => return Ok(Entry::BelowSymlink(names[..=i].join("/"))),
            Kind::File => return Ok(Entry::Absent),
        }
    }
    let (file, kind) = match look_last(&directory, &names) {
        Ok(found) => found,
        Err(e) if is_absent(&e) => return Ok(Entry::Absent),
        Err(e) => return Err(e),
    };
    Ok(match kind {
        Kind::Link => Entry::Symlink,
        Kind::Directory => Entry::Directory,
        Kind::File => Entry::File(file),
    })
}

/// Opens the last of `path` (the names from the root) in `directory` with `access`, as itself, and says what it is. A
/// reparse point that is not a link (a cloud placeholder, a deduplicated file) gets its data from a filter that opening
/// it as itself bypasses, so it is opened again through the filter: the handle returned is that one.
fn look(directory: &File, path: &[&str], access: u32) -> io::Result<(File, Kind)> {
    let name = path.last().expect("a path has a name");
    let file = open_below(directory, name, true, access)?;
    let (kind, filtered) = classify(&file)?;
    if filtered {
        return Ok((read_through(directory, name, &file, path, access)?, kind));
    }
    Ok((file, kind))
}

/// [`look`] at the end of the path, where a file is opened for reading. A directory that denies reading is still a
/// directory, so when reading is refused the name is looked at again for searching.
fn look_last(directory: &File, path: &[&str]) -> io::Result<(File, Kind)> {
    match look(directory, path, FILE_GENERIC_READ) {
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => match look(directory, path, SEARCH) {
            Ok((found, Kind::Directory)) => Ok((found, Kind::Directory)),
            _ => Err(e),
        },
        other => other,
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(SEARCH)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

/// Opens `name`, one name and no path, in `directory` with `access`. With `itself`, whatever is at `name` is opened as
/// itself: a link is not followed and a cloud placeholder's filter does not run. Without it, the open goes through them.
fn open_below(directory: &File, name: &str, itself: bool, access: u32) -> io::Result<File> {
    let wide: Vec<u16> = name.encode_utf16().collect();
    let bytes = u16::try_from(wide.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "the name is too long for Windows"))?;
    let name = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: wide.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle() as HANDLE,
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let options = if itself { FILE_OPEN_REPARSE_POINT } else { 0 };
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: an all-zero IO_STATUS_BLOCK is the state the call expects to overwrite.
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: `attributes` points at `name`, which points at `wide`; all three outlive the call, and `directory` is an
    // open directory handle. The call writes only `handle` and `status_block`.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: a plain conversion of a status code.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: the call succeeded, so `handle` is an open handle that nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

/// Opens `name` again, this time through the filter of a reparse point that is not a link, and returns that handle.
/// `first` is the handle of the open as itself; the two must be the same file, or the name was swapped in between.
fn read_through(directory: &File, name: &str, first: &File, path: &[&str], access: u32) -> io::Result<File> {
    let through = open_below(directory, name, false, access)?;
    if file_id(first)? != file_id(&through)? {
        return Err(io::Error::other(format!(
            "`{}` changed while it was being opened",
            path.join("/").escape_debug()
        )));
    }
    Ok(through)
}

/// The volume and the 128-bit file ID: what says two handles are the same file.
fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO {
        VolumeSerialNumber: 0,
        FileId: Default::default(),
    };
    // SAFETY: `info` is a FILE_ID_INFO of the size passed, and the handle is open.
    let read = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

/// What `file` is, and whether it is a reparse point that is not a link.
fn classify(file: &File) -> io::Result<(Kind, bool)> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO {
        FileAttributes: 0,
        ReparseTag: 0,
    };
    // SAFETY: `info` is a FILE_ATTRIBUTE_TAG_INFO of the size passed, and the handle is open.
    let read = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if read == 0 {
        return Err(io::Error::last_os_error());
    }
    let reparse_point = info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    if reparse_point && info.ReparseTag & NAME_SURROGATE != 0 {
        return Ok((Kind::Link, false));
    }
    let kind = if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Kind::Directory
    } else {
        Kind::File
    };
    Ok((kind, reparse_point))
}
