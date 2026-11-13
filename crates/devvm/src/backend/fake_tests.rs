use std::path::Path;

use super::{FakeBackend, FakeStorage, FakeTarget};
use crate::backend::contract::{self, ContractTarget};
use crate::store::layers::Built;
use crate::store::layers::layers_tests::{env, key, request};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::RecordingHost;
use crate::{Arch, ErrorKind};

#[skuld::test]
fn fake_storage_writes_only_into_staging(#[fixture(unicode_home)] home: &Path) {
    contract::storage_writes_only_into_staging(&FakeTarget::new(), home);
}

#[skuld::test]
fn fake_storage_refuses_what_the_storage_contract_refuses(#[fixture(unicode_home)] home: &Path) {
    contract::storage_refuses_what_its_contract_refuses(&FakeStorage::default(), home);
}

#[skuld::test]
fn fake_launch_names_the_layer_a_disk_sits_on_when_it_is_gone(#[fixture(unicode_home)] home: &Path) {
    let target = FakeTarget::new();
    let env = env(home);
    let parent = target.parent_layer(&env.store);
    let spec = target.spec(home, 0);
    std::fs::create_dir_all(spec.disk.parent().unwrap()).unwrap();
    target
        .backend()
        .storage()
        .create_instance_disk(parent.tip().dir(), &spec.disk, Some(1 << 20))
        .unwrap();
    std::fs::remove_file(parent.tip().dir().path().join("disk.qcow2")).unwrap();

    let error = target.backend().launch(&spec).err().expect("the backing layer is gone");
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.message().contains("disk.qcow2"), "{}", error.message());
    assert_eq!(target.launches(), 0, "nothing was spawned");
}

#[skuld::test]
fn fake_import_base_makes_the_image_the_layers_disk(#[fixture(unicode_home)] home: &Path) {
    let target = FakeTarget::new();
    let env = env(home);
    let image = home.join("downloaded.img");
    std::fs::write(&image, b"an image").unwrap();

    let imported = env
        .store
        .build(&request(key(5)), None, |staging| {
            target
                .backend()
                .storage()
                .import_base(&image, Arch::Amd64, staging.dir)?;
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
        .create_instance_disk(imported.tip().dir(), &spec.disk, Some(1 << 20))
        .unwrap();
}

#[skuld::test]
fn fake_import_base_of_a_missing_image_is_an_io_error(#[fixture(unicode_home)] home: &Path) {
    let target = FakeTarget::new();
    let env = env(home);
    let built = env.store.build(&request(key(5)), None, |staging| {
        target
            .backend()
            .storage()
            .import_base(&home.join("missing.img"), Arch::Amd64, staging.dir)?;
        Ok(Built::default())
    });
    assert!(
        matches!(built, Err(e) if e.kind() == ErrorKind::Io),
        "there is no image"
    );
}

/// The claim's identity is the held file's, not the one a name leads to after the hold is made.
#[skuld::test]
fn fake_claim_takes_its_identity_from_the_hold_not_from_a_lookup_by_name(#[fixture(unicode_home)] home: &Path) {
    let host = RecordingHost::new();
    host.fail_file_id();
    let host: &'static RecordingHost = Box::leak(Box::new(host));

    let claim = FakeBackend::new(host).claim(&home.join("a.sock.hold")).unwrap();

    assert_eq!(claim.id, crate::host::current().file_id(&claim.path).unwrap());
}
