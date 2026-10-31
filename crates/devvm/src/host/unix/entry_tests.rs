use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::host::{Entry, Host};
use crate::test_support::fixtures::unicode_home;

use super::super::UnixHost;

fn open(root: &Path, rel: &str) -> Entry {
    UnixHost
        .open_entry(root, rel)
        .unwrap_or_else(|e| panic!("open_entry({rel}): {e}"))
}

/// Makes a FIFO at `path`.
fn make_fifo(path: &Path) {
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("a path without NUL");
    // SAFETY: `name` is a NUL-terminated path that outlives the call.
    let made = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo: {}", std::io::Error::last_os_error());
}

/// A FIFO with no writer would block an `open` for reading that does not say `O_NONBLOCK`: if this returns, it did not.
#[skuld::test]
fn a_fifo_is_other_and_opening_it_does_not_wait_for_a_writer(#[fixture(unicode_home)] home: &Path) {
    make_fifo(&home.join("pipe"));
    assert!(matches!(open(home, "pipe"), Entry::Other("a FIFO")));
}

#[skuld::test]
fn a_socket_is_other(#[fixture(unicode_home)] home: &Path) {
    let _listener = std::os::unix::net::UnixListener::bind(home.join("sock")).expect("a socket");
    assert!(matches!(open(home, "sock"), Entry::Other("a socket")));
}

#[skuld::test]
fn a_fifo_in_the_middle_of_a_path_makes_what_is_below_it_absent(#[fixture(unicode_home)] home: &Path) {
    make_fifo(&home.join("pipe"));
    assert!(matches!(open(home, "pipe/below"), Entry::Absent));
}
