use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use crate::ErrorKind;
use crate::instance::registry::registry_tests::{create, fixture, lock_files, new_instance};
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
    assert_eq!(lock_files(&fx).len(), 2, "the lock files stay");

    drop(held);
    let op = dir.op_exclusive().unwrap();
    dir.destroy(op).unwrap();
    assert!(names(&fx.env.dirs.instances()).is_empty());
    assert!(lock_files(&fx).is_empty());
}

/// Creates `disk.img`, keeps it open without sharing, and fails: the half-built directory cannot be deleted.
struct HoldsItsDisk {
    held: std::sync::Mutex<Option<std::fs::File>>,
}

impl crate::backend::Storage for HoldsItsDisk {
    fn create_instance_disk(
        &self,
        _layer: &crate::store::layers::LayerDir,
        into: &Path,
        _size: u64,
    ) -> crate::Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(into)
            .unwrap();
        *self.held.lock().unwrap() = Some(file);
        Err(crate::Error::new(ErrorKind::Io, "disk creation failed"))
    }

    fn commit_layer(&self, _: &Path, _: &crate::store::layers::LayerDir) -> crate::Result<()> {
        unreachable!()
    }

    fn import_base(&self, _: &Path, _: crate::Arch, _: &crate::store::layers::LayerDir) -> crate::Result<()> {
        unreachable!()
    }
}

#[skuld::test]
fn a_failed_create_that_cannot_delete_its_half_built_directory_says_so(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let storage = HoldsItsDisk {
        held: std::sync::Mutex::new(None),
    };
    let guard = fx.env.store.begin_record().unwrap();

    let error = fx
        .registry
        .create(new_instance(&fx, Some("foo"), &storage), &guard)
        .err()
        .expect("the disk cannot be made");

    assert_eq!(
        error.message(),
        "disk creation failed",
        "the creation's own error comes back"
    );
    let left = names(&fx.env.dirs.instances());
    assert_eq!(left.len(), 1, "{left:?}");
    let messages = fx.progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.contains(&left[0]) && m.contains("could not be deleted")),
        "the leftover is not announced: {messages:?}"
    );

    drop(storage);
    fx.registry.reap();
    assert!(
        names(&fx.env.dirs.instances()).is_empty(),
        "the next command deletes it"
    );
}
