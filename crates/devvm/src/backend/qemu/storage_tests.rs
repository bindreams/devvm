//! The storage's own checks, which run before `qemu-img` does: they need no QEMU.

use std::path::Path;

use super::{DISK, QemuStorage};
use crate::ErrorKind;
use crate::backend::Storage;
use crate::host;
use crate::store::layers::layers_tests::{env, key, request};
use crate::store::layers::{Built, ChainLease};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::qemu::lazy_at;

/// A storage whose QEMU does not exist: an operation that reaches `qemu-img` fails naming the missing tool.
fn storage_without_qemu(home: &Path) -> QemuStorage {
    QemuStorage::new(lazy_at(&home.join("no-qemu").join("bin")), host::current())
}

/// A layer that holds a disk and, if `vars`, the firmware variables.
fn layer(home: &Path, vars: bool) -> ChainLease {
    env(home)
        .store
        .build(&request(key(1)), None, |staging| {
            std::fs::write(staging.dir.path().join(DISK), b"disk").unwrap();
            if vars {
                std::fs::write(staging.dir.path().join(super::VARS), b"vars").unwrap();
            }
            Ok(Built::default())
        })
        .unwrap()
}

#[skuld::test]
fn an_instance_disk_over_a_layer_without_its_variables_is_refused_naming_the_file(
    #[fixture(unicode_home)] home: &Path,
) {
    let storage = storage_without_qemu(home);
    let layer = layer(home, false);
    let into = home.join("inst").join("disk.qcow2");
    std::fs::create_dir_all(into.parent().unwrap()).unwrap();

    let error = storage
        .create_instance_disk(layer.tip().dir(), &into, 1 << 20)
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Store, "{error}");
    assert!(error.message().contains("vars.fd"), "{error}");
    assert!(
        error
            .message()
            .contains(&layer.tip().dir().path().display().to_string()),
        "{error}"
    );
    assert!(!into.exists(), "a disk was made for a machine that cannot start");
}

#[skuld::test]
fn committing_an_instance_without_its_variables_is_refused_naming_the_file(#[fixture(unicode_home)] home: &Path) {
    let storage = storage_without_qemu(home);
    let instance = home.join("inst").join("disk.qcow2");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();
    std::fs::write(&instance, b"disk").unwrap();
    let staging = env(home).store.build(&request(key(2)), None, |staging| {
        let error = storage.commit_layer(&instance, staging.dir).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Store, "{error}");
        assert!(error.message().contains("vars.fd"), "{error}");
        assert!(
            !staging.dir.path().join(DISK).exists(),
            "a layer was started without its variables"
        );
        Ok(Built::default())
    });

    staging.unwrap();
}

#[skuld::test]
fn an_instance_disk_with_no_directory_is_refused_naming_it_before_qemu_img_runs(#[fixture(unicode_home)] home: &Path) {
    let storage = storage_without_qemu(home);
    let layer = layer(home, true);

    let error = storage
        .create_instance_disk(layer.tip().dir(), Path::new(""), 1 << 20)
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("instance disk"), "{error}");
}

#[skuld::test]
fn committing_an_instance_disk_with_no_directory_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let storage = storage_without_qemu(home);
    env(home)
        .store
        .build(&request(key(3)), None, |staging| {
            let error = storage.commit_layer(Path::new(""), staging.dir).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
            assert!(error.message().contains("instance disk"), "{error}");
            Ok(Built::default())
        })
        .unwrap();
}
