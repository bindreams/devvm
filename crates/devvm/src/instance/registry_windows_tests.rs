use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use crate::ErrorKind;
use crate::instance::registry::registry_tests::{create, fixture};
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::anna_home;

#[skuld::test]
fn destroy_with_a_refused_rename_deletes_nothing_and_the_retry_succeeds(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    // Another process has a file inside open without sharing: Windows refuses to rename the directory.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(dir.path().join("disk.img"))
        .unwrap();

    let op = dir.op_exclusive().unwrap();
    let error = dir.destroy(op).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("foo"), "{}", error.message());
    assert!(dir.record().is_ok(), "nothing was deleted");
    assert_eq!(names(&fx.env.dirs.instance_locks()).len(), 2, "the lock files stay");

    drop(held);
    let op = dir.op_exclusive().unwrap();
    dir.destroy(op).unwrap();
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(names(&fx.env.dirs.instance_locks()).is_empty());
}
