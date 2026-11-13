//! The extraction command of a `copy` step, wrapped as the guest wraps it and run by a real `sh` with a real `tar`. They
//! spawn both, so they run in the deputy's runner and CI.

use std::path::Path;
use std::process::ExitStatus;

use crate::guest::GuestOs;
use crate::guest::linux::LinuxGuest;
use crate::host::symlink_metadata_if_present;
use crate::test_support::fixtures::unicode_home;

/// A destination that does not exist yet and that a shell would split, expand or run if the command quoted it badly.
const DESTINATION: &str = "sp ace;semi &amp 'quote' \"double\" $(dollar) `tick` *star";

/// Runs the extraction of `archive` into `destination` as the guest would: the string the guest's wrapping makes of
/// the command, handed to `sh -c` as sshd hands it to the account's shell.
fn extract(destination: &Path, archive: &Path) -> ExitStatus {
    let command = LinuxGuest
        .extract_command(destination.to_str().unwrap(), archive.to_str().unwrap())
        .unwrap();
    std::process::Command::new("sh")
        .args(["-c", &LinuxGuest.wrap(&command).unwrap()])
        .status()
        .unwrap()
}

#[skuld::test]
fn extracting_a_good_archive_makes_the_destination_fills_it_and_removes_the_archive(
    #[fixture(unicode_home)] home: &Path,
) {
    let archive = home.join("devvm-copy-x.tar");
    let mut builder = tar::Builder::new(std::fs::File::create(&archive).unwrap());
    for (name, content) in [("a.txt", "tracked"), ("dir/b.txt", "also tracked")] {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        builder.append_data(&mut header, name, content.as_bytes()).unwrap();
    }
    builder.finish().unwrap();
    drop(builder);
    let destination = home.join(DESTINATION);

    let status = extract(&destination, &archive);

    assert!(status.success(), "{status}");
    assert_eq!(std::fs::read_to_string(destination.join("a.txt")).unwrap(), "tracked");
    assert_eq!(
        std::fs::read_to_string(destination.join("dir/b.txt")).unwrap(),
        "also tracked"
    );
    assert!(
        symlink_metadata_if_present(&archive).unwrap().is_none(),
        "the archive is left"
    );
}

#[skuld::test]
fn extracting_a_corrupt_archive_removes_it_and_fails(#[fixture(unicode_home)] home: &Path) {
    let archive = home.join("devvm-copy-x.tar");
    std::fs::write(&archive, vec![b'x'; 2048]).unwrap();

    let status = extract(&home.join(DESTINATION), &archive);

    assert!(!status.success(), "the status of tar is lost: {status}");
    assert!(
        symlink_metadata_if_present(&archive).unwrap().is_none(),
        "a failed extraction leaves the archive"
    );
}
