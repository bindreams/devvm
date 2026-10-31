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
    FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileAttributeTagInfo,
    GetFileInformationByHandleEx,
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
    let (last, parents) = names.split_last().expect("a path has a name");
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(root)?;
    for (i, name) in parents.iter().enumerate() {
        let next = match open_below(&directory, name) {
            Ok(next) => next,
            Err(e) if is_absent(&e) => return Ok(Entry::Absent),
            Err(e) => return Err(e),
        };
        match kind(&next)? {
            Kind::Directory => directory = next,
            Kind::Link => return Ok(Entry::BelowSymlink(names[..=i].join("/"))),
            Kind::File => return Ok(Entry::Absent),
        }
    }
    let file = match open_below(&directory, last) {
        Ok(file) => file,
        Err(e) if is_absent(&e) => return Ok(Entry::Absent),
        Err(e) => return Err(e),
    };
    Ok(match kind(&file)? {
        Kind::Link => Entry::Symlink,
        Kind::Directory => Entry::Directory,
        Kind::File => Entry::File(file),
    })
}

/// Opens `name`, one name and no path, in `directory` for reading. A link at `name` is opened itself.
fn open_below(directory: &File, name: &str) -> io::Result<File> {
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
    let mut handle: HANDLE = std::ptr::null_mut();
    // SAFETY: an all-zero IO_STATUS_BLOCK is the state the call expects to overwrite.
    let mut status_block: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
    // SAFETY: `attributes` points at `name`, which points at `wide`; all three outlive the call, and `directory` is an
    // open directory handle. The call writes only `handle` and `status_block`.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_GENERIC_READ,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
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

fn kind(file: &File) -> io::Result<Kind> {
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
    Ok(
        if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && info.ReparseTag & NAME_SURROGATE != 0 {
            Kind::Link
        } else if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            Kind::Directory
        } else {
            Kind::File
        },
    )
}
