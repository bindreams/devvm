use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
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

/// Directories that can be searched but not read (`chmod 300`, `chmod 100`) still lead to what is in them, as a path
/// does. Mode bits do not bind root, so the test refuses to run as root.
#[skuld::test]
fn a_directory_that_can_be_searched_but_not_read_is_walked(#[fixture(unicode_home)] home: &Path) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "directory modes do not bind root: run this test as another user"
    );
    for mode in [0o300, 0o100] {
        let name = format!("d{mode:o}");
        let directory = home.join(&name);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("f.txt"), b"x").unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode)).unwrap();
        let found = UnixHost.open_entry(home, &format!("{name}/f.txt"));
        // The temporary directory cannot be removed with the mode left as it is.
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(found, Ok(Entry::File(_))), "{name}: {found:?}");
    }
}
