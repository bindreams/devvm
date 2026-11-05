//! AF_UNIX: how long a path binds (bytes, not characters), and what a socket file does while its listener lives or
//! after it is gone.
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

use crate::out::{err, kind_of, row, show, show_with};

pub fn run(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
    std::fs::create_dir_all(root).unwrap();
    for unit in ["a", "\u{e9}", "\u{20ac}", "\u{1F600}"] {
        max_bind(&root.join("len"), unit);
    }
    stat_calls(&root.join("stat"), false);
    stat_calls(&root.join("stat-stale"), true);
    delete_while_listening(&root.join("delete"));
    rename_while_listening(&root.join("rename"));
    missing_and_refused(&root.join("refused"));
    let _ = std::fs::remove_dir_all(root);
}

/// The longest name of `unit` repeated that binds under `dir`, in bytes of the whole path.
fn max_bind(dir: &Path, unit: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let base = dir.as_os_str().as_encoded_bytes().len() + 1;
    let (mut last_ok, mut first_err) = (None, None);
    for n in 1..=1000usize {
        let path = dir.join(unit.repeat(n));
        let bytes = base + n * unit.len();
        match UnixListener::bind(&path) {
            Ok(listener) => {
                drop(listener);
                let there = std::fs::symlink_metadata(&path).is_ok();
                let _ = std::fs::remove_file(&path);
                last_ok = Some((n, bytes, there));
            }
            Err(e) => {
                first_err = Some((n, bytes, err(&e)));
                break;
            }
        }
    }
    row(
        "sockets-path-length",
        format!(
            "unit={unit:?} ({} bytes each) dir_bytes={} last_ok(n, path_bytes, file_exists_at_path)={last_ok:?} \
             first_refusal(n, path_bytes, error)={first_err:?}",
            unit.len(),
            base - 1
        ),
    );
}

/// What the std calls say about a socket's name, while it listens or after its listener is gone.
fn stat_calls(dir: &Path, stale: bool) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("s");
    let listener = UnixListener::bind(&path).unwrap();
    if stale {
        drop(listener);
    }
    let text = format!(
        "symlink_metadata={} metadata={} exists={} try_exists={} is_file={} is_dir={} open_read={} connect={}",
        show_with(&std::fs::symlink_metadata(&path), |m| kind_of(m).to_owned()),
        show_with(&std::fs::metadata(&path), |m| kind_of(m).to_owned()),
        path.exists(),
        show_with(&path.try_exists(), |b| b.to_string()),
        path.is_file(),
        path.is_dir(),
        show(&std::fs::File::open(&path)),
        show(&UnixStream::connect(&path)),
    );
    row("sockets-stat", format!("state={} :: {text}", if stale { "listener dropped, file left" } else { "listening" }));
}

fn delete_while_listening(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("s");
    let first = UnixListener::bind(&path).unwrap();
    let removed = std::fs::remove_file(&path);
    let after = show(&std::fs::symlink_metadata(&path));
    let connect = show(&UnixStream::connect(&path));
    let second = UnixListener::bind(&path);
    let second_connect = show(&UnixStream::connect(&path));
    row(
        "sockets-delete",
        format!(
            "listening, remove_file by name = {} ; name afterwards = {after} ; connect by name = {connect} ; \
             bind the same name again = {} ; connect = {second_connect}",
            show(&removed),
            show(&second)
        ),
    );
    drop((first, second));
    // A stale file: bind over it.
    let stale_path = dir.join("stale");
    drop(UnixListener::bind(&stale_path).unwrap());
    row(
        "sockets-delete",
        format!("bind over a stale socket file = {}", show(&UnixListener::bind(&stale_path))),
    );
}

fn rename_while_listening(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("s");
    let moved = dir.join("s2");
    let listener = UnixListener::bind(&path).unwrap();
    let renamed = std::fs::rename(&path, &moved);
    row(
        "sockets-rename",
        format!(
            "listening, rename the socket file = {} ; connect to the new name = {} ; connect to the old name = {}",
            show(&renamed),
            show(&UnixStream::connect(&moved)),
            show(&UnixStream::connect(&path))
        ),
    );
    drop(listener);
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let listener = UnixListener::bind(sub.join("s")).unwrap();
    let sub2 = dir.join("sub2");
    let renamed = std::fs::rename(&sub, &sub2);
    row(
        "sockets-rename",
        format!(
            "listening, rename the directory holding the socket = {} ; connect at the new location = {}",
            show(&renamed),
            show(&UnixStream::connect(sub2.join("s")))
        ),
    );
    drop(listener);
}

fn missing_and_refused(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    row(
        "sockets-connect",
        format!("missing file = {}", show(&UnixStream::connect(dir.join("nothing")))),
    );
    row(
        "sockets-connect",
        format!("missing parent = {}", show(&UnixStream::connect(dir.join("nodir").join("s")))),
    );
    std::fs::write(dir.join("plain"), b"x").unwrap();
    row(
        "sockets-connect",
        format!("a regular file = {}", show(&UnixStream::connect(dir.join("plain")))),
    );
    row(
        "sockets-bind",
        format!("onto a regular file = {}", show(&UnixListener::bind(dir.join("plain")))),
    );
    row(
        "sockets-bind",
        format!("missing parent = {}", show(&UnixListener::bind(dir.join("nodir").join("s")))),
    );
}
