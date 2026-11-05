//! Throwaway measurement of OS facts for devvm's "a failed check is not gone / delete-pending / lock identity" class.
//! The lines starting `RESULT` are the evidence. One root directory per run (`fs`), so a run is repeated per file
//! system. Never run on the owner's PC except `fs` under `.tmp/claude` with SPIKE_ACLS and SPIKE_RESERVED unset.
//!
//! - `fs <label> <dir>`: lookup/refusal/pending matrix, lock identity, directory rename/delete, races, file ids.
//! - `sockets <label> <dir>`: AF_UNIX path lengths and a socket file's life.
//! - `once`: facts that do not depend on the file system.
mod calls;
mod cases;
mod dirs;
mod identity;
mod matrix;
mod out;
mod race;
mod util;
mod sockets;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod win;

use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("fs") => {
            out::set_label(&args[2]);
            let root = Path::new(&args[3]);
            std::fs::create_dir_all(root).unwrap();
            let root = std::path::absolute(root).unwrap();
            preamble(&root);
            guarded("lookup", || matrix::run(&root.join("lookup")));
            guarded("identity", || identity::locks(&root.join("identity")));
            guarded("ids", || identity::ids(&root.join("ids")));
            guarded("dirs", || dirs::run(&root.join("dirs")));
            guarded("race", || race::run(&root.join("race")));
            guarded("fsops", || fsops(&root.join("fsops")));
        }
        Some("sockets") => {
            out::set_label(&args[2]);
            let root = std::path::absolute(&args[3]).unwrap();
            std::fs::create_dir_all(&root).unwrap();
            preamble(&root);
            guarded("sockets", || sockets::run(&root.join("sockets")));
        }
        Some("once") => {
            out::set_label("-");
            #[cfg(windows)]
            win::once();
        }
        other => panic!("unknown mode {other:?}"),
    }
}

/// A panic in one group does not stop the others.
fn guarded(group: &str, f: impl FnOnce()) {
    if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        let text = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        out::row(group, format!("PANIC {text}"));
    }
}

fn preamble(root: &Path) {
    out::row("env", format!("os={} root={}", std::env::consts::OS, root.display()));
    #[cfg(unix)]
    out::row("env", format!("euid={}", unsafe { libc::geteuid() }));
    #[cfg(windows)]
    win::preamble(root);
}

#[cfg(unix)]
fn fsops(dir: &Path) {
    unix::run(dir);
}

#[cfg(windows)]
fn fsops(dir: &Path) {
    win::fsops(dir);
}
