//! Windows-only: delete-pending states, file ids, handle views, rename-by-handle name forms, the volume's flags.
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, DeleteFileW, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FileDispositionInfo, FileDispositionInfoEx, FileIdInfo, FileRenameInfo, FileRenameInfoEx, FileStandardInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationByHandleW, RemoveDirectoryW,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::WindowsProgramming::FILE_RENAME_FLAG_POSIX_SEMANTICS;
use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows_sys::core::GUID;

use crate::cases::{Case, Made, case};
use crate::out::{err, row, show};
use crate::util::{Share, lock_text, open_dir, open_read, touch};

const ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn last() -> io::Error {
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

fn bool_result(ok: i32) -> io::Result<()> {
    if ok != 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

// Marking ---------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum Mark {
    Posix,
    PosixIgnoreReadonly,
    Legacy,
}

/// `file` must be open with `DELETE`. No fallback: the raw error is the evidence.
pub fn mark(file: &File, how: Mark) -> io::Result<()> {
    match how {
        Mark::Posix | Mark::PosixIgnoreReadonly => {
            let extra = if matches!(how, Mark::PosixIgnoreReadonly) {
                FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE
            } else {
                0
            };
            let info = FILE_DISPOSITION_INFO_EX {
                Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS | extra,
            };
            // SAFETY: the buffer is a FILE_DISPOSITION_INFO_EX of the size passed.
            bool_result(unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfoEx,
                    (&raw const info).cast(),
                    size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
                )
            })
        }
        Mark::Legacy => {
            let info = FILE_DISPOSITION_INFO { DeleteFile: true };
            // SAFETY: the buffer is a FILE_DISPOSITION_INFO of the size passed.
            bool_result(unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfo,
                    (&raw const info).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                )
            })
        }
    }
}

fn open_for_delete(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .access_mode(DELETE | FILE_READ_ATTRIBUTES)
        .share_mode(ALL)
        .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

fn delete_file_w(path: &Path) -> io::Result<()> {
    let name = wide(path);
    // SAFETY: NUL-terminated.
    bool_result(unsafe { DeleteFileW(name.as_ptr()) })
}

fn remove_directory_w(path: &Path) -> io::Result<()> {
    let name = wide(path);
    // SAFETY: NUL-terminated.
    bool_result(unsafe { RemoveDirectoryW(name.as_ptr()) })
}

fn made(path: std::path::PathBuf, note: String, keep: Vec<Box<dyn std::any::Any>>) -> io::Result<Made> {
    let mut m = Made::at(path)?;
    m.note = note;
    m.keep = keep;
    Ok(m)
}

/// The states a file or directory name can be in while its deletion is under way.
pub fn pending_cases() -> Vec<Case> {
    vec![
        case("pending-posix-marker-handle-open", |s| {
            let p = s.join("f");
            touch(&p)?;
            let marker = open_for_delete(&p)?;
            let r = mark(&marker, Mark::PosixIgnoreReadonly);
            made(p, format!("POSIX mark, marking handle kept open: {}", show(&r)), vec![Box::new(marker)])
        }),
        case("pending-posix-marker-handle-open-plus-other-handle", |s| {
            let p = s.join("f");
            touch(&p)?;
            let other = open_read(&p, Share::All)?;
            let marker = open_for_delete(&p)?;
            let r = mark(&marker, Mark::PosixIgnoreReadonly);
            made(
                p,
                format!("POSIX mark, marking handle and another handle kept open: {}", show(&r)),
                vec![Box::new(marker), Box::new(other)],
            )
        }),
        case("pending-posix-marker-closed-other-handle-open", |s| {
            let p = s.join("f");
            touch(&p)?;
            let other = open_read(&p, Share::All)?;
            let marker = open_for_delete(&p)?;
            let r = mark(&marker, Mark::PosixIgnoreReadonly);
            drop(marker);
            made(p, format!("POSIX mark, marking handle closed, another handle kept: {}", show(&r)), vec![Box::new(other)])
        }),
        case("pending-legacy-mark-other-handle-open", |s| {
            let p = s.join("f");
            touch(&p)?;
            let other = open_read(&p, Share::All)?;
            let marker = open_for_delete(&p)?;
            let r = mark(&marker, Mark::Legacy);
            drop(marker);
            made(p, format!("non-POSIX mark, marking handle closed, another handle kept: {}", show(&r)), vec![Box::new(other)])
        }),
        case("pending-deletefilew-other-handle-open", |s| {
            let p = s.join("f");
            touch(&p)?;
            let other = open_read(&p, Share::All)?;
            let r = delete_file_w(&p);
            made(p, format!("DeleteFileW, another handle (share all) kept: {}", show(&r)), vec![Box::new(other)])
        }),
        case("deletefilew-other-handle-without-share-delete", |s| {
            let p = s.join("f");
            touch(&p)?;
            let other = open_read(&p, Share::NoDelete)?;
            let r = delete_file_w(&p);
            made(p, format!("DeleteFileW, another handle without delete sharing kept: {}", show(&r)), vec![Box::new(other)])
        }),
        case("pending-dir-removedirectoryw-handle-open", |s| {
            let d = s.join("d");
            std::fs::create_dir(&d)?;
            let handle = open_dir(&d, Share::All)?;
            let r = remove_directory_w(&d);
            made(d, format!("RemoveDirectoryW, a handle on the directory (share all) kept: {}", show(&r)), vec![Box::new(handle)])
        }),
        case("pending-dir-posix-marked-handle-open", |s| {
            let d = s.join("d");
            std::fs::create_dir(&d)?;
            let marker = open_for_delete(&d)?;
            let r = mark(&marker, Mark::Posix);
            made(d, format!("POSIX mark of a directory, marking handle kept: {}", show(&r)), vec![Box::new(marker)])
        }),
        case("dir-with-pending-child", |s| {
            let d = s.join("d");
            std::fs::create_dir(&d)?;
            let f = d.join("f");
            touch(&f)?;
            let other = open_read(&f, Share::All)?;
            let marker = open_for_delete(&f)?;
            let r = mark(&marker, Mark::Legacy);
            drop(marker);
            made(d, format!("child marked non-POSIX with another handle kept: {}", show(&r)), vec![Box::new(other)])
        }),
    ]
}

// Permissions -----------------------------------------------------------------------------------------------------

pub fn deny_everyone(path: &Path) -> io::Result<()> {
    icacls(path, &["/deny", "*S-1-1-0:(F)"])
}

pub fn allow_everyone(path: &Path) {
    let _ = icacls(path, &["/remove:d", "*S-1-1-0"]);
}

fn icacls(path: &Path, args: &[&str]) -> io::Result<()> {
    let out = std::process::Command::new("icacls").arg(path).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("icacls {args:?}: {}", String::from_utf8_lossy(&out.stdout))))
    }
}

// Handles ---------------------------------------------------------------------------------------------------------

pub fn file_id(file: &File) -> io::Result<String> {
    let mut info = FILE_ID_INFO {
        VolumeSerialNumber: 0,
        FileId: unsafe { std::mem::zeroed() },
    };
    // SAFETY: the buffer is a FILE_ID_INFO of the size passed.
    bool_result(unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    })?;
    let id: String = info.FileId.Identifier.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{:x}:{id}", info.VolumeSerialNumber))
}

/// What the holder of a handle sees about the file: link count, delete-pending flag, the path it is at now.
pub fn holder_view(file: &File) -> String {
    let mut info = FILE_STANDARD_INFO {
        AllocationSize: 0,
        EndOfFile: 0,
        NumberOfLinks: 0,
        DeletePending: false,
        Directory: false,
    };
    // SAFETY: the buffer is a FILE_STANDARD_INFO of the size passed.
    let standard = bool_result(unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&raw mut info).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    });
    let standard = match standard {
        Ok(()) => format!("NumberOfLinks={} DeletePending={}", info.NumberOfLinks, info.DeletePending),
        Err(e) => err(&e),
    };
    format!("{standard} final_path={}", match final_path(file) {
        Ok(p) => p,
        Err(e) => err(&e),
    })
}

fn final_path(file: &File) -> io::Result<String> {
    // SAFETY: a null buffer of size 0 asks for the length.
    let needed = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), null_mut(), 0, 0) };
    if needed == 0 {
        return Err(last());
    }
    let mut buffer = vec![0u16; needed as usize + 1];
    // SAFETY: the buffer holds `buffer.len()` units.
    let written = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), buffer.as_mut_ptr(), buffer.len() as u32, 0) };
    if written == 0 || written as usize >= buffer.len() {
        return Err(last());
    }
    Ok(std::ffi::OsString::from_wide(&buffer[..written as usize]).to_string_lossy().into_owned())
}

pub fn preamble(root: &Path) {
    let Ok(dir) = open_dir(root, Share::All) else { return };
    let mut name = [0u16; 64];
    let mut flags = 0u32;
    // SAFETY: the buffers are as long as stated.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            dir.as_raw_handle(),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        row("env", format!("volume information: {}", err(&last())));
        return;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(0);
    let known = [
        (0x0000_0400u32, "POSIX_UNLINK_RENAME"),
        (0x0040_0000, "HARD_LINKS"),
        (0x0000_0080, "REPARSE_POINTS"),
        (0x0100_0000, "OPEN_BY_FILE_ID"),
        (0x0000_0004, "UNICODE_ON_DISK"),
        (0x0000_0001, "CASE_SENSITIVE_SEARCH"),
    ];
    let names: Vec<&str> = known.iter().filter(|(bit, _)| flags & bit != 0).map(|(_, n)| *n).collect();
    row(
        "env",
        format!("filesystem={} flags=0x{flags:08x} {names:?}", String::from_utf16_lossy(&name[..len])),
    );
}

// Per file system -------------------------------------------------------------------------------------------------

pub fn fsops(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let a = dir.join("a");
    touch(&a).unwrap();
    row("fsops", format!("hard_link(existing, new) = {}", show(&std::fs::hard_link(&a, dir.join("b")))));

    for (label, how) in [("POSIX", Mark::PosixIgnoreReadonly), ("POSIX only", Mark::Posix), ("legacy", Mark::Legacy)] {
        let p = dir.join("mark");
        touch(&p).unwrap();
        let marker = open_for_delete(&p).unwrap();
        let r = mark(&marker, how);
        drop(marker);
        row("fsops", format!("disposition {label} on an open-for-delete handle = {}", show(&r)));
        let _ = std::fs::remove_file(&p);
    }

    let f = OpenOptions::new().read(true).write(true).open(&a).unwrap();
    let g = OpenOptions::new().read(true).write(true).open(&a).unwrap();
    row("fsops", format!("LockFileEx: first handle {} ; second handle {}", lock_text(f.try_lock()), lock_text(g.try_lock())));
    drop((f, g));

    // FILE_RENAME_INFO.FileName in several forms, with and without POSIX semantics.
    let abs = std::path::absolute(dir).unwrap();
    for posix in [true, false] {
        let forms: [(&str, String); 4] = [
            ("absolute C:\\...", abs.join("t1").display().to_string()),
            ("verbatim \\\\?\\C:\\...", format!(r"\\?\{}", abs.join("t2").display())),
            ("bare name", "t3".to_owned()),
            ("absolute with forward slashes", abs.join("t4").display().to_string().replace('\\', "/")),
        ];
        for (label, name) in forms {
            let p = dir.join("renamed-from");
            touch(&p).unwrap();
            let handle = open_for_delete(&p).unwrap();
            let r = rename_by_handle(&handle, &name, posix);
            drop(handle);
            let landed = ["t1", "t2", "t3", "t4"].iter().find(|n| abs.join(n).exists() || Path::new(n).exists());
            row(
                "fsops",
                format!(
                    "FILE_RENAME_INFO name form [{label}] posix_semantics={posix} = {} ; file landed at {landed:?}",
                    show(&r)
                ),
            );
            for n in ["t1", "t2", "t3", "t4"] {
                let _ = std::fs::remove_file(abs.join(n));
                let _ = std::fs::remove_file(n);
            }
            let _ = std::fs::remove_file(&p);
        }
    }
}

fn rename_by_handle(file: &File, name: &str, posix: bool) -> io::Result<()> {
    let name: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = size_of::<u16>() * name.len();
    let mut buffer = vec![0u64; (offset_of!(FILE_RENAME_INFO, FileName) + name_bytes).div_ceil(8) + 1];
    let size = (buffer.len() * 8) as u32;
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: zeroed, aligned, and large enough for the header and the name.
    unsafe {
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (&raw mut (*info).FileName).cast::<u16>(), name.len());
        if posix {
            (*info).Anonymous.Flags = FILE_RENAME_FLAG_POSIX_SEMANTICS;
        }
    }
    let class = if posix { FileRenameInfoEx } else { FileRenameInfo };
    // SAFETY: the handle has DELETE access; the buffer is a FILE_RENAME_INFO of the size passed.
    bool_result(unsafe { SetFileInformationByHandle(file.as_raw_handle(), class, info.cast(), size) })
}

// Once ------------------------------------------------------------------------------------------------------------

pub fn once() {
    let mut raw: *mut u16 = null_mut();
    let bogus = GUID {
        data1: 0xdead_beef,
        data2: 0x1234,
        data3: 0x5678,
        data4: [1, 2, 3, 4, 5, 6, 7, 8],
    };
    // SAFETY: as the product calls it; a failed call's buffer is freed as documented.
    let status = unsafe { SHGetKnownFolderPath(&bogus, KF_FLAG_DEFAULT as u32, null_mut(), &mut raw) };
    unsafe { CoTaskMemFree(raw.cast()) };
    let as_os = io::Error::from_raw_os_error(status);
    let low = io::Error::from_raw_os_error(status & 0xffff);
    row(
        "known-folder",
        format!(
            "SHGetKnownFolderPath(unknown id) = HRESULT 0x{:08x} ; from_raw_os_error(hresult): kind={:?} text={:?} ; \
             from_raw_os_error(low 16 bits): kind={:?} text={:?}",
            status as u32,
            as_os.kind(),
            as_os.to_string(),
            low.kind(),
            low.to_string()
        ),
    );
    let mut raw: *mut u16 = null_mut();
    // SAFETY: as the product calls it.
    let status = unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT as u32, null_mut(), &mut raw) };
    let path = if status == 0 {
        let len = (0..).take_while(|&i| unsafe { *raw.add(i) } != 0).count();
        // SAFETY: NUL-terminated by the call.
        let units = unsafe { std::slice::from_raw_parts(raw, len) };
        std::ffi::OsString::from_wide(units).to_string_lossy().into_owned()
    } else {
        String::new()
    };
    unsafe { CoTaskMemFree(raw.cast()) };
    row("known-folder", format!("SHGetKnownFolderPath(ProgramData) = 0x{:08x} {path}", status as u32));
}
