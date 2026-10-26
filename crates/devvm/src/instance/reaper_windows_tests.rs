use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use crate::engine::engine_tests::dead_machine;
use crate::instance::record::InstanceId;
use crate::instance::registry::registry_tests::{create, fixture, fixture_with};
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::anna_home;
use crate::test_support::stub_backend::{Attach, StubBackend};

fn open_without_sharing(path: &Path) -> std::fs::File {
    std::fs::OpenOptions::new().read(true).share_mode(0).open(path).unwrap()
}

#[skuld::test]
fn a_failed_delete_is_a_warning_and_the_next_reap_finishes_it(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    let dead = fx.env.dirs.instances().join(format!(".dead-{}", InstanceId::random()));
    std::fs::create_dir_all(&dead).unwrap();
    std::fs::write(dead.join("disk.img"), "x").unwrap();
    let held = open_without_sharing(&dead.join("disk.img"));

    let report = fx.registry.reap();
    assert_eq!(report.warnings.len(), 1, "{report:?}");
    assert!(report.warnings[0].contains(".dead-"), "{}", report.warnings[0]);
    assert!(dead.exists());

    drop(held);
    let report = fx.registry.reap();
    assert!(report.warnings.is_empty(), "{report:?}");
    assert!(!dead.exists());
}

#[skuld::test]
fn a_refused_quarantine_rename_is_a_warning_and_the_next_reap_moves_it(#[fixture(anna_home)] home: &Path) {
    let fx = fixture(home);
    drop(create(&fx, Some("foo")));
    let dir = fx.env.dirs.instances().join("foo");
    std::fs::write(dir.join("record.json"), "{ torn").unwrap();
    let held = open_without_sharing(&dir.join("disk.img"));

    let report = fx.registry.reap();
    assert_eq!(report.warnings.len(), 1, "{report:?}");
    assert!(report.warnings[0].contains("foo"), "{}", report.warnings[0]);
    assert!(report.quarantined.is_empty());
    assert!(dir.join("record.json").is_file(), "nothing was lost");

    drop(held);
    let report = fx.registry.reap();
    assert_eq!(report.quarantined.len(), 1, "{report:?}");
    assert_eq!(names(&fx.env.dirs.instance_quarantine().join("instances")).len(), 1);
}

#[skuld::test]
fn an_instance_whose_files_cannot_be_updated_or_removed_is_warned_about_and_reaped_next_time(
    #[fixture(anna_home)] home: &Path,
) {
    let stub = StubBackend::new(Attach::Running);
    let fx = fixture_with(home, Some(stub.clone()));
    let owned = create(&fx, Some("foo"));
    let record = owned.dir().path().join("record.json");
    let op = owned.dir().op_exclusive().unwrap();
    owned
        .dir()
        .update(&op, &mut |r| r.machine = Some(dead_machine(Vec::new())))
        .unwrap();
    drop((op, owned));
    // A program outside devvm opens the record without sharing just as the VM is killed.
    let held = std::sync::Arc::new(std::sync::Mutex::new(None));
    let holder = held.clone();
    let mut fired = false;
    stub.on_stop(move || {
        if !std::mem::replace(&mut fired, true) {
            *holder.lock().unwrap() = Some(open_without_sharing(&record));
        }
    });

    let report = fx.registry.reap();

    assert!(report.reaped.is_empty(), "{report:?}");
    let about = |text: &str| report.warnings.iter().any(|w| w.contains("foo") && w.contains(text));
    assert!(about("cannot update instance"), "{:?}", report.warnings);
    assert!(about("cannot remove instance"), "{:?}", report.warnings);
    assert_eq!(names(&fx.env.dirs.instances()), ["foo"], "nothing was deleted");

    *held.lock().unwrap() = None;
    let report = fx.registry.reap();
    assert_eq!(report.reaped, ["foo"], "{:?}", report.warnings);
}
