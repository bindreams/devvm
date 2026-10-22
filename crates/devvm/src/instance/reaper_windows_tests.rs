use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use crate::instance::record::InstanceId;
use crate::instance::registry::registry_tests::{create, fixture};
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::anna_home;

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
