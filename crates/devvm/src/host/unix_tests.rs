use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use rustix::io::Errno;

use super::{UnixHost, lacks_a_rename_that_refuses_to_replace, publish_with};
use crate::host::{Entry, Host};
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn only_a_missing_rename_is_told_from_a_refusal_of_this_one() {
    for lacking in [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP] {
        assert!(lacks_a_rename_that_refuses_to_replace(lacking), "{lacking}");
    }
    for refusal in [
        Errno::EXIST,
        Errno::NOTEMPTY,
        Errno::NOENT,
        Errno::ACCESS,
        Errno::PERM,
        Errno::XDEV,
        Errno::ROFS,
        Errno::NOSPC,
        Errno::IO,
    ] {
        assert!(!lacks_a_rename_that_refuses_to_replace(refusal), "{refusal}");
    }
}

fn dir_with_a_file(home: &Path, name: &str) -> std::path::PathBuf {
    let dir = home.join(name);
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("held"), name).unwrap();
    dir
}

#[skuld::test]
fn a_rename_the_file_system_lacks_falls_back_to_claiming_the_name(#[fixture(unicode_home)] home: &Path) {
    for lacking in [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP] {
        let (from, to) = (dir_with_a_file(home, "from"), home.join("to"));
        publish_with(&from, &to, |_, _| Err(lacking)).unwrap();
        assert!(!from.exists(), "{lacking}");
        assert_eq!(std::fs::read_to_string(to.join("held")).unwrap(), "from", "{lacking}");
        std::fs::remove_dir_all(&to).unwrap();
    }
}

#[skuld::test]
fn the_fallback_still_refuses_a_name_that_is_taken(#[fixture(unicode_home)] home: &Path) {
    let (from, to) = (dir_with_a_file(home, "from"), home.join("taken"));
    std::fs::create_dir(&to).unwrap();
    let error = publish_with(&from, &to, |_, _| Err(Errno::NOSYS)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
    assert!(from.join("held").exists());
}

#[skuld::test]
fn a_refusal_of_this_rename_is_the_error_and_nothing_is_claimed(#[fixture(unicode_home)] home: &Path) {
    let (from, to) = (dir_with_a_file(home, "from"), home.join("to"));
    let error = publish_with(&from, &to, |_, _| Err(Errno::ACCESS)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{error}");
    assert!(!to.exists(), "a name was claimed");
    assert!(from.join("held").exists());
}

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

#[skuld::test]
fn a_fifo_is_other_and_a_socket_is_other(#[fixture(unicode_home)] home: &Path) {
    make_fifo(&home.join("pipe"));
    let _listener = std::os::unix::net::UnixListener::bind(home.join("sock")).expect("a socket");
    assert!(matches!(open(home, "pipe"), Entry::Other("a FIFO")));
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
