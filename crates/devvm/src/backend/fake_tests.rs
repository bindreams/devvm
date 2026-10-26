use std::path::Path;

use super::FakeTarget;
use crate::ErrorKind;
use crate::backend::contract::{self, ContractTarget};
use crate::store::layers::layers_tests::env;
use crate::test_support::fixtures::anna_home;

#[skuld::test]
fn fake_storage_writes_only_into_staging(#[fixture(anna_home)] home: &Path) {
    contract::storage_writes_only_into_staging(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_launch_names_the_layer_a_disk_sits_on_when_it_is_gone(#[fixture(anna_home)] home: &Path) {
    let target = FakeTarget::new();
    let env = env(home);
    let parent = target.parent_layer(&env.store);
    let spec = target.spec(home, 0);
    std::fs::create_dir_all(spec.disk.parent().unwrap()).unwrap();
    target
        .backend()
        .storage()
        .create_instance_disk(parent.tip().dir(), &spec.disk, 1 << 20)
        .unwrap();
    std::fs::remove_file(parent.tip().dir().path().join("disk.qcow2")).unwrap();

    let error = target.backend().launch(&spec).err().expect("the backing layer is gone");
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.message().contains("disk.qcow2"), "{}", error.message());
    assert_eq!(target.launches(), 0, "nothing was spawned");
}
