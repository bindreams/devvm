use std::path::Path;

use super::FakeTarget;
use crate::backend::contract::{self, ContractTarget};
use crate::store::layers::layers_tests::{env, key, request};
use crate::store::layers::Built;
use crate::{Arch, ErrorKind};
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

#[skuld::test]
fn fake_import_base_makes_the_image_the_layers_disk(#[fixture(anna_home)] home: &Path) {
    let target = FakeTarget::new();
    let env = env(home);
    let image = home.join("downloaded.img");
    std::fs::write(&image, b"an image").unwrap();

    let imported = env
        .store
        .build(&request(key(5)), None, |staging| {
            target.backend().storage().import_base(&image, Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .unwrap();

    let disk = imported.tip().dir().path().join("disk.qcow2");
    assert_eq!(std::fs::read(&disk).unwrap(), b"an image");
    assert_eq!(std::fs::read(&image).unwrap(), b"an image", "the download is only read");
    // An instance disk can sit on it.
    let spec = target.spec(home, 0);
    std::fs::create_dir_all(spec.disk.parent().unwrap()).unwrap();
    target
        .backend()
        .storage()
        .create_instance_disk(imported.tip().dir(), &spec.disk, 1 << 20)
        .unwrap();
}

#[skuld::test]
fn fake_import_base_of_a_missing_image_is_an_io_error(#[fixture(anna_home)] home: &Path) {
    let target = FakeTarget::new();
    let env = env(home);
    let error = env
        .store
        .build(&request(key(5)), None, |staging| {
            target
                .backend()
                .storage()
                .import_base(&home.join("missing.img"), Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .err()
        .expect("there is no image");
    assert_eq!(error.kind(), ErrorKind::Io);
}
