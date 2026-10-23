use std::path::Path;

use crate::ErrorKind;
use crate::instance::name::InstanceName;
use crate::instance::reaper::ReapReport;
use crate::instance::registry::registry_tests::{create, fixture};
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::anna_home;

fn instance_is_intact(path: &Path) {
    assert!(path.join("record.json").is_file() && path.join("owner.json").is_file());
    assert!(path.join("disk.img").is_file());
}

#[skuld::test]
fn a_stale_verdict_without_an_id_never_moves_a_valid_instance_that_took_the_name(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    // The verdict "unreadable, id unknown" was taken when `foo` was being removed; another agent has since created a
    // new `foo`, whose owner holds its lock.
    let owned = create(&fx, Some("foo"));
    let path = fx.env.dirs.instances().join("foo");
    let mut report = ReapReport::default();

    fx.registry.quarantine("foo", &path, None, "stale", &mut report);

    assert!(
        report.quarantined.is_empty() && report.warnings.is_empty(),
        "{report:?}"
    );
    instance_is_intact(&path);
    assert!(names(&fx.env.dirs.instance_quarantine().join("instances")).is_empty());
    drop(owned);
}

#[skuld::test]
fn a_stale_verdict_with_another_id_never_moves_the_instance_at_the_path(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let old = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let old_id = *old.id();
    let op = old.op_exclusive().unwrap();
    old.destroy(op).unwrap();
    let new = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let path = fx.env.dirs.instances().join("foo");
    let mut report = ReapReport::default();

    fx.registry.quarantine("foo", &path, Some(old_id), "stale", &mut report);

    assert!(
        report.quarantined.is_empty() && report.warnings.is_empty(),
        "{report:?}"
    );
    instance_is_intact(&path);
    assert!(new.record().is_ok());
}

#[skuld::test]
fn a_stale_verdict_for_a_directory_that_is_now_valid_moves_nothing(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let path = fx.env.dirs.instances().join("foo");
    let mut report = ReapReport::default();

    fx.registry
        .quarantine("foo", &path, Some(*dir.id()), "stale", &mut report);

    assert!(
        report.quarantined.is_empty() && report.warnings.is_empty(),
        "{report:?}"
    );
    instance_is_intact(&path);
}

#[skuld::test]
fn a_vanished_path_is_success_with_no_warning(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let id = *dir.id();
    let path = fx.env.dirs.instances().join("foo");
    let op = dir.op_exclusive().unwrap();
    dir.destroy(op).unwrap();

    for id in [None, Some(id)] {
        let mut report = ReapReport::default();
        fx.registry.quarantine("foo", &path, id, "stale", &mut report);
        assert!(
            report.quarantined.is_empty() && report.warnings.is_empty(),
            "{id:?}: {report:?}"
        );
    }
    assert!(fx.progress.messages().is_empty(), "{:?}", fx.progress.messages());
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

#[skuld::test]
fn a_moved_instance_directory_is_quarantined_not_trusted(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let root = fx.env.dirs.instances();
    std::fs::rename(root.join("foo"), root.join("bar")).unwrap();
    drop(dir);

    // Before the reaper runs, `bar` is not served on the strength of a record that says `foo`.
    let error = fx.registry.open(&InstanceName::parse("bar").unwrap()).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Store);
    assert_eq!(
        error.message(),
        format!(
            "the record in the directory of instance bar is for an instance called foo ({}); the next devvm command \
             moves the directory to quarantine",
            root.join("bar").join("record.json").display()
        )
    );
    assert!(fx.registry.list().unwrap().is_empty());

    let report = fx.registry.reap();
    assert_eq!(report.quarantined.len(), 1, "{report:?}");
    assert!(names(&root).is_empty());
    instance_is_intact(&report.quarantined[0]);
}

#[skuld::test]
fn a_copied_instance_directory_is_quarantined_and_the_original_keeps_working(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dir = create(&fx, Some("foo")).make_ownerless(&mut || Ok(())).unwrap();
    let root = fx.env.dirs.instances();
    copy_dir(&root.join("foo"), &root.join("bar"));

    let report = fx.registry.reap();
    assert_eq!(report.quarantined.len(), 1, "{report:?}");
    assert_eq!(names(&root), ["foo"]);
    assert!(report.quarantined[0].to_string_lossy().contains("bar-"), "{report:?}");
    let op = dir.op_exclusive().unwrap();
    dir.destroy(op).unwrap();
    assert!(names(&root).is_empty());
}
