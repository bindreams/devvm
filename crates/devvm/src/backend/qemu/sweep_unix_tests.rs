use std::path::Path;

use super::sweep_tests::{STEM_A, run_with_a_lock, sweep, sweeping_a_link_touches_nothing_and_warns};
use crate::backend::qemu::qmp::QmpEndpoint;
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn sweeping_a_run_dir_that_is_a_symlink_touches_nothing_and_warns(#[fixture(unicode_home)] home: &Path) {
    sweeping_a_link_touches_nothing_and_warns(home, |target, link| {
        std::os::unix::fs::symlink(target, link).unwrap();
    });
}

/// The scenario of a hostile neighbour: a regular `.sock` names the launch, and its claim file is a link to a file of
/// the user's. The sweep must not open the file through it, and must not delete it.
#[skuld::test]
fn a_claim_file_that_is_a_symlink_is_not_opened_through_by_the_sweep() {
    let run = run_with_a_lock();
    let victim = run.path().join("victim");
    std::fs::write(&victim, b"theirs").unwrap();
    let endpoint = QmpEndpoint::of_launch(run.path(), STEM_A);
    std::fs::write(endpoint.path(), b"socket").unwrap();
    let claim = endpoint.hold_path();
    std::os::unix::fs::symlink(&victim, &claim).unwrap();

    let warnings = sweep(run.path());

    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(&claim.display().to_string()), "{warnings:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"theirs");
    assert!(
        std::fs::symlink_metadata(&claim).is_ok(),
        "the link is left where it is"
    );
}

/// A runtime directory whose path is not valid UTF-8 is swept like any other: no name goes through a lossy string.
/// Linux only: APFS refuses a name that is not UTF-8.
#[cfg(target_os = "linux")]
#[skuld::test]
fn a_run_dir_whose_path_is_not_utf8_is_swept() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    use super::sweep_tests::Machine;
    use crate::host;

    let parent = crate::test_support::qemu::scratch_run();
    let run = parent.path().join(OsStr::from_bytes(b"r\xff"));
    host::current().create_runtime_dir(&run).unwrap();
    drop(crate::store::lock::FileLock::open(&run.join("runtime.lock")).unwrap());
    let mut machine = Machine::new(&run, STEM_A);
    machine.dies();

    assert_eq!(sweep(&run), Vec::<String>::new());

    assert!(machine.none_exist());
}
