//! The storage's own checks, which run before `qemu-img` does: they need no QEMU.

use std::path::Path;

use super::{DISK, QemuStorage, VARS};
use crate::backend::{Patch, Storage};
use crate::host;
use crate::store::layers::layers_tests::{env, key, request};
use crate::store::layers::{Built, ChainLease};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::qemu::{lazy_at, refused_without_a_disk};
use crate::{Arch, ErrorKind};

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
                std::fs::write(staging.dir.path().join(VARS), b"vars").unwrap();
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
        .create_instance_disk(layer.tip().dir(), &into, Some(1 << 20))
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

/// `u64::MAX - 510` is the smallest size whose round-up to 512 overflows `u64`; `(1 << 51) + 1` is above qcow2's largest
/// virtual size, 2 PiB.
#[skuld::test]
fn a_disk_qemu_cannot_make_is_refused_as_a_recipe_error_before_qemu_img_runs(#[fixture(unicode_home)] home: &Path) {
    let storage = storage_without_qemu(home);
    let layer = layer(home, true);

    for size in [u64::MAX, u64::MAX - 510, (1 << 51) + 1] {
        let error = refused_without_a_disk(&storage, &layer, home, Some(size), ErrorKind::Recipe);

        assert!(error.message().contains(&size.to_string()), "{size}: {error}");
        assert!(error.message().contains("resources.disk"), "{size}: {error}");
    }
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
        .create_instance_disk(layer.tip().dir(), Path::new(""), Some(1 << 20))
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

#[skuld::test]
fn the_real_storage_refuses_what_the_storage_contract_refuses_before_qemu_img_runs(
    #[fixture(unicode_home)] home: &Path,
) {
    crate::backend::contract::storage_refuses_what_its_contract_refuses(&storage_without_qemu(home), home);
}

/// Same sizes as for an instance disk: `made_size` is the one rule.
#[skuld::test]
fn a_blank_disk_over_the_qcow2_limit_is_refused_as_recipe_before_qemu_img_runs(#[fixture(unicode_home)] home: &Path) {
    let storage = storage_without_qemu(home);
    env(home)
        .store
        .build(&request(key(4)), None, |staging| {
            for size in [u64::MAX, u64::MAX - 510, (1 << 51) + 1] {
                let error = storage.create_blank_disk(size, Arch::Amd64, staging.dir).unwrap_err();

                assert_eq!(error.kind(), ErrorKind::Recipe, "{size}: {error}");
                assert!(error.message().contains(&size.to_string()), "{size}: {error}");
                assert!(!error.message().contains("resources.disk"), "{size}: {error}");
                assert!(!staging.dir.path().join(DISK).exists(), "{size}: a disk was made");
                assert!(!staging.dir.path().join(VARS).exists(), "{size}: variables were copied");
            }
            Ok(Built::default())
        })
        .unwrap();
}

#[skuld::test]
fn a_missing_backing_file_is_refused_naming_it_before_qemu_img_runs(#[fixture(unicode_home)] home: &Path) {
    let storage = storage_without_qemu(home);
    let backing = home.join("no-such.iso");
    let into = home.join("overlay.qcow2");
    let patches = [Patch {
        offset: 0,
        bytes: vec![1],
    }];

    let error = storage.overlay_with_patches(&backing, &patches, &into).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(error.message().contains("no-such.iso"), "{error}");
    assert!(!into.exists(), "an overlay was made over nothing");
}
