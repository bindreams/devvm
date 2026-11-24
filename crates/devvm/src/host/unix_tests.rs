use std::fs::{File, FileTimes};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::io::Errno;

use super::{UnixHost, lacks_a_rename_that_refuses_to_replace, lacks_hard_links, publish_with, set_modified};
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

/// `EACCES` is a `PermissionDenied` too, from a directory that cannot be written or a security module, and is no
/// statement about hard links.
#[skuld::test]
fn only_a_file_system_without_hard_links_is_told_from_a_refusal_of_this_link() {
    for lacking in [Errno::PERM, Errno::NOTSUP, Errno::OPNOTSUPP] {
        assert!(lacks_hard_links(&std::io::Error::from(lacking)), "{lacking}");
    }
    for refusal in [
        Errno::ACCESS,
        Errno::EXIST,
        Errno::NOENT,
        Errno::NOSPC,
        Errno::ROFS,
        Errno::XDEV,
        Errno::IO,
    ] {
        assert!(!lacks_hard_links(&std::io::Error::from(refusal)), "{refusal}");
    }
    assert!(!lacks_hard_links(&std::io::Error::other("no OS code")));
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

/// A file or directory its owner cannot read (`chmod 200`, `chmod 300`) takes a time. Mode bits do not bind root, so the
/// test refuses to run as root.
#[skuld::test]
fn set_mtime_sets_the_time_of_what_its_owner_cannot_read(#[fixture(unicode_home)] home: &Path) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "modes do not bind root: run this test as another user"
    );
    let file = home.join("write-only.txt");
    std::fs::write(&file, b"x").unwrap();
    let directory = home.join("write-only-dir");
    std::fs::create_dir(&directory).unwrap();
    for (path, mode) in [(&file, 0o200), (&directory, 0o300)] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        let set = UnixHost.set_mtime(path, 1_000_000_000);
        let since = std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        // The temporary directory cannot be removed with the modes left as they are.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        set.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(since.as_secs(), 1_000_000_000, "{}", path.display());
    }
}

/// Seconds since 1970 of a file's access time or modification time, which `stat` reads without touching either.
fn seconds(time: std::io::Result<SystemTime>) -> u64 {
    time.unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// The access time stays what it was: only the modification time is set.
#[skuld::test]
fn set_modified_leaves_the_access_time_alone(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("read.txt");
    std::fs::write(&file, b"x").unwrap();
    let accessed = UNIX_EPOCH + Duration::from_secs(500_000_000);
    File::open(&file)
        .unwrap()
        .set_times(FileTimes::new().set_accessed(accessed))
        .unwrap();
    set_modified(&file, UNIX_EPOCH + Duration::from_secs(1_000_000_000)).unwrap();
    let metadata = std::fs::metadata(&file).unwrap();
    assert_eq!(seconds(metadata.accessed()), 500_000_000);
    assert_eq!(seconds(metadata.modified()), 1_000_000_000);
}

/// A link is followed: the time lands on what it points at, and the link keeps its own.
#[skuld::test]
fn set_modified_sets_the_time_of_the_target_of_a_link(#[fixture(unicode_home)] home: &Path) {
    let (target, link) = (home.join("target.txt"), home.join("link.txt"));
    std::fs::write(&target, b"x").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let link_time = seconds(std::fs::symlink_metadata(&link).unwrap().modified());
    set_modified(&link, UNIX_EPOCH + Duration::from_secs(1_000_000_000)).unwrap();
    assert_eq!(seconds(std::fs::metadata(&target).unwrap().modified()), 1_000_000_000);
    assert_eq!(seconds(std::fs::symlink_metadata(&link).unwrap().modified()), link_time);
}

/// A FIFO takes a time without waiting for a peer. A regression that opens the FIFO blocks in `open` with no one on the
/// other end, so this test hangs instead of failing: a hang here is the failure, and nothing caps it.
#[skuld::test]
fn set_modified_sets_the_time_of_a_fifo_without_blocking(#[fixture(unicode_home)] home: &Path) {
    let fifo = home.join("pipe");
    make_fifo(&fifo);
    set_modified(&fifo, UNIX_EPOCH + Duration::from_secs(1_000_000_000)).unwrap();
    assert_eq!(seconds(std::fs::metadata(&fifo).unwrap().modified()), 1_000_000_000);
}

// The runtime directory -----------------------------------------------------------------------------------------------

#[skuld::test]
fn the_runtime_directory_is_made_for_the_owner_alone(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("run");
    UnixHost.create_runtime_dir(&dir).unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
}

#[skuld::test]
fn a_symlink_at_the_runtime_dir_path_is_refused(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = home.join("run");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = UnixHost.create_runtime_dir(&link).unwrap_err();
    assert!(error.to_string().contains("link"), "{error}");
}

#[skuld::test]
fn a_symlink_at_the_runtime_dir_path_is_refused_by_the_check_that_makes_nothing(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = home.join("run");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let error = UnixHost.accept_runtime_dir(&link).unwrap_err();
    assert!(error.to_string().contains("link"), "{error}");
}

/// A parent that cannot be searched hides the directory; the error names the directory. Mode bits do not bind root, so
/// the test refuses to run as root.
#[skuld::test]
fn a_runtime_dir_behind_an_unsearchable_parent_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "directory modes do not bind root: run this test as another user"
    );
    let parent = home.join("locked");
    std::fs::create_dir(&parent).unwrap();
    let dir = parent.join("run");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o0)).unwrap();
    let error = UnixHost.accept_runtime_dir(&dir).unwrap_err();
    // The temporary directory cannot be removed with the mode left as it is.
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied, "{error}");
    assert!(
        error
            .to_string()
            .starts_with(&format!("cannot look at {}", dir.display())),
        "{error}"
    );
}

#[skuld::test]
fn file_id_of_a_symlink_is_the_link_not_its_target(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::write(&target, b"").unwrap();
    let link = home.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_ne!(UnixHost.file_id(&link).unwrap(), UnixHost.file_id(&target).unwrap());
}

// Holds ---------------------------------------------------------------------------------------------------------------

#[skuld::test]
fn a_new_hold_is_private_to_the_user(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("x.sock.hold");
    let hold = UnixHost.hold_new(&path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    drop(hold);
}

/// A name that is a link is not opened through: what it leads to is somebody else's file, and holding it would lock
/// that file against its users.
#[skuld::test]
fn holding_an_existing_name_that_is_a_symlink_opens_nothing_through_it(#[fixture(unicode_home)] home: &Path) {
    let victim = home.join("victim");
    std::fs::write(&victim, b"theirs").unwrap();
    let link = home.join("x.sock.hold");
    std::os::unix::fs::symlink(&victim, &link).unwrap();

    let error = UnixHost.hold_existing(&link).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
    assert!(error.to_string().contains("is a link"), "{error}");

    assert_eq!(std::fs::read(&victim).unwrap(), b"theirs");
    let file = std::fs::File::open(&victim).unwrap();
    file.try_lock().expect("nobody holds the victim");
}

#[skuld::test]
fn the_final_path_of_a_held_file_is_refused_when_its_name_now_leads_to_another_file(
    #[fixture(unicode_home)] home: &Path,
) {
    let path = home.join("a.iso");
    std::fs::write(&path, b"windows").unwrap();
    let held = UnixHost.open_held_read(&path).unwrap();
    std::fs::write(home.join("b.iso"), b"somewhere else").unwrap();
    std::fs::rename(home.join("b.iso"), &path).unwrap();

    let error = UnixHost.final_path_of_open(&held, &path).unwrap_err();
    assert!(error.to_string().contains("a.iso"), "{error}");
}
