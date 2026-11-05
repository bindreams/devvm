//! Unix-only calls: `link(2)`, a rename that refuses to replace, `flock` on this file system.
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::out::{row, show};
use crate::util::{lock_text, touch};

pub fn run(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let (a, b) = (dir.join("a"), dir.join("b"));
    touch(&a).unwrap();
    row("fsops", format!("hard_link(existing, new) = {}", show(&std::fs::hard_link(&a, &b))));
    row("fsops", format!("hard_link(existing, existing) = {}", show(&std::fs::hard_link(&a, &b))));
    let _ = std::fs::remove_file(&b);

    for target in ["absent", "empty dir", "non-empty dir", "file"] {
        let (src_dir, to) = (dir.join("src-dir"), dir.join("to"));
        for p in [&src_dir, &to] {
            let _ = std::fs::remove_dir_all(p);
            let _ = std::fs::remove_file(p);
        }
        std::fs::create_dir(&src_dir).unwrap();
        match target {
            "empty dir" => std::fs::create_dir(&to).unwrap(),
            "non-empty dir" => {
                std::fs::create_dir(&to).unwrap();
                touch(&to.join("f")).unwrap();
            }
            "file" => touch(&to).unwrap(),
            _ => {}
        }
        row(
            "fsops",
            format!("rename-without-replacing a dir onto {target} = {}", show(&no_replace(&src_dir, &to))),
        );
    }

    let f = std::fs::OpenOptions::new().read(true).write(true).open(&a).unwrap();
    let g = std::fs::OpenOptions::new().read(true).write(true).open(&a).unwrap();
    row(
        "fsops",
        format!("flock: first open {} ; second open {}", lock_text(f.try_lock()), lock_text(g.try_lock())),
    );
}

#[cfg(target_os = "linux")]
fn no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (CString::new(from.as_os_str().as_bytes())?, CString::new(to.as_os_str().as_bytes())?);
    const RENAME_NOREPLACE: libc::c_uint = 1;
    // SAFETY: both are NUL-terminated and live for the call.
    let status = unsafe {
        libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, from.as_ptr(), libc::AT_FDCWD, to.as_ptr(), RENAME_NOREPLACE)
    };
    if status == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn no_replace(from: &Path, to: &Path) -> io::Result<()> {
    unsafe extern "C" {
        fn renamex_np(from: *const libc::c_char, to: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
    }
    const RENAME_EXCL: libc::c_uint = 4;
    let (from, to) = (CString::new(from.as_os_str().as_bytes())?, CString::new(to.as_os_str().as_bytes())?);
    // SAFETY: both are NUL-terminated and live for the call.
    let status = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), RENAME_EXCL) };
    if status == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}
