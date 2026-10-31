//! The qcow2 storage against the sidecar's `qemu-img`. All of it runs under the Unicode test directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::Storage;
use crate::backend::qemu::sidecar::QemuInstall;
use crate::backend::qemu::storage::{DISK, QemuStorage, VARS};
use crate::host;
use crate::store::layers::layers_tests::{Env, env, key, request};
use crate::store::layers::{Built, ChainLease};
use crate::test_support::fixtures::{UNICODE_DIR, unicode_home};
use crate::test_support::qemu::{lazy_for, qemu_img, qemu_img_info, qemu_sidecar, read_through};
use crate::{Arch, ErrorKind};

const BASE_LEN: usize = 1 << 20;

fn storage(install: &QemuInstall) -> QemuStorage {
    QemuStorage::new(lazy_for(install), host::current())
}

/// Deterministic, non-zero bytes, so a read through the backing chain is told apart from zeros.
fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i * 7) as u8).wrapping_add(seed) | 1).collect()
}

fn write_raw(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A root layer imported from a raw image of `pattern(BASE_LEN, 1)`.
fn base_layer(world: &Env, storage: &QemuStorage, home: &Path) -> ChainLease {
    let raw = write_raw(home, "base.raw", &pattern(BASE_LEN, 1));
    world
        .store
        .build(&request(key(1)), None, |staging| {
            storage.import_base(&raw, Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .unwrap()
}

fn disk_of(layer: &ChainLease) -> PathBuf {
    layer.tip().dir().path().join(DISK)
}

#[skuld::test]
fn create_instance_disk_is_a_cow_overlay_of_the_layer(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    assert!(base.tip().dir().path().to_string_lossy().contains(UNICODE_DIR));

    let instance = home.join("inst").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();
    storage
        .create_instance_disk(base.tip().dir(), &instance, 8 << 20)
        .unwrap();

    let info = qemu_img_info(install, &instance, false);
    assert_eq!(info["format"], "qcow2");
    assert_eq!(info["virtual-size"], 8 << 20);
    assert!(info["actual-size"].as_u64().unwrap() < BASE_LEN as u64, "{info}");
    let backing = info["full-backing-filename"].as_str().unwrap();
    assert!(Path::new(backing).ends_with(DISK), "{backing}");
    // A read through the overlay sees the layer's bytes, then zeros past the end of the layer.
    let seen = read_through(install, &instance, home);
    assert_eq!(seen.len(), 8 << 20);
    assert_eq!(seen[..BASE_LEN], pattern(BASE_LEN, 1)[..]);
    assert!(seen[BASE_LEN..].iter().all(|byte| *byte == 0));
}

#[skuld::test]
fn disk_size_larger_than_the_backing_image_is_accepted(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let instance = home.join("big.img");

    storage
        .create_instance_disk(base.tip().dir(), &instance, 64 << 20)
        .unwrap();

    assert_eq!(qemu_img_info(install, &instance, false)["virtual-size"], 64 << 20);
}

/// Six layers on the root: each one's backing path is absolute, so the chain's path length does not grow with its depth.
#[skuld::test]
fn a_chain_of_layers_resolves_however_deep_it_is(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let base_before = std::fs::read(disk_of(&base)).unwrap();

    let mut tip = base;
    for depth in 1..=6u8 {
        let instance = home.join(format!("instance-{depth}")).join("disk.img");
        std::fs::create_dir_all(instance.parent().unwrap()).unwrap();
        storage
            .create_instance_disk(tip.tip().dir(), &instance, 8 << 20)
            .unwrap();
        let instance_before = std::fs::read(&instance).unwrap();
        tip = world
            .store
            .build(&request(key(10 + depth)), Some(&tip), |staging| {
                storage.commit_layer(&instance, staging.dir)?;
                assert!(staging.dir.path().join(DISK).is_file());
                assert!(staging.dir.path().join(VARS).is_file());
                Ok(Built::default())
            })
            .unwrap();
        assert_eq!(
            std::fs::read(&instance).unwrap(),
            instance_before,
            "the instance disk is only read; the layer is a copy"
        );

        let chain = qemu_img_info(install, &disk_of(&tip), true);
        let chain = chain.as_array().unwrap();
        assert_eq!(chain.len(), depth as usize + 1, "{chain:?}");
        let backing = chain[0]["backing-filename"].as_str().unwrap();
        assert!(Path::new(backing).is_absolute(), "{backing}");
        assert_eq!(
            read_through(install, &disk_of(&tip), home)[..BASE_LEN],
            pattern(BASE_LEN, 1)[..],
            "depth {depth}"
        );
    }

    let root = tip.layers().first().expect("a chain has a root");
    assert_eq!(
        std::fs::read(root.dir().path().join(DISK)).unwrap(),
        base_before,
        "a parent layer is never written"
    );
}

#[skuld::test]
fn committing_an_instance_disk_with_no_backing_file_is_refused(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let instance = home.join("alone").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();
    std::fs::write(instance.parent().unwrap().join(VARS), b"vars").unwrap();
    qemu_img(
        install,
        &[
            "create".as_ref(),
            "-f".as_ref(),
            "qcow2".as_ref(),
            instance.as_os_str(),
            "1M".as_ref(),
        ],
    );

    let error = world
        .store
        .build(&request(key(9)), None, |staging| {
            storage.commit_layer(&instance, staging.dir)?;
            Ok(Built::default())
        })
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.message().contains("no backing file"), "{error}");
}

#[skuld::test]
fn a_disk_smaller_than_its_backing_image_is_refused_by_qemu_img_and_nothing_is_left(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let instance = home.join("small").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    let error = storage
        .create_instance_disk(base.tip().dir(), &instance, (BASE_LEN / 2) as u64)
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().starts_with("qemu-img create"), "{error}");
    assert!(!instance.parent().unwrap().join(VARS).exists(), "{error}");
}

#[skuld::test]
fn import_base_copies_qcow2_and_converts_raw(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let bytes = pattern(BASE_LEN, 3);
    let raw = write_raw(home, "source.raw", &bytes);
    let qcow2 = home.join("source.qcow2");
    qemu_img(
        install,
        &[
            "convert".as_ref(),
            "-O".as_ref(),
            "qcow2".as_ref(),
            raw.as_os_str(),
            qcow2.as_os_str(),
        ],
    );

    let copied = world
        .store
        .build(&request(key(4)), None, |staging| {
            storage.import_base(&qcow2, Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .unwrap();
    assert_eq!(
        std::fs::read(disk_of(&copied)).unwrap(),
        std::fs::read(&qcow2).unwrap(),
        "a qcow2 base is copied as it is"
    );

    let converted = world
        .store
        .build(&request(key(5)), None, |staging| {
            storage.import_base(&raw, Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .unwrap();
    assert_eq!(qemu_img_info(install, &disk_of(&converted), false)["format"], "qcow2");
    assert_eq!(read_through(install, &disk_of(&converted), home), bytes);
}

#[skuld::test]
fn import_base_adds_the_variables_template_of_the_architecture(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let raw = write_raw(home, "source.raw", &pattern(4096, 5));

    for (n, arch, template) in [
        (6, Arch::Amd64, "edk2-i386-vars.fd"),
        (7, Arch::Arm64, "edk2-arm-vars.fd"),
    ] {
        let layer = world
            .store
            .build(&request(key(n)), None, |staging| {
                storage.import_base(&raw, arch, staging.dir)?;
                Ok(Built::default())
            })
            .unwrap();
        assert_eq!(
            std::fs::read(layer.tip().dir().path().join(VARS)).unwrap(),
            std::fs::read(install.share.join(template)).unwrap(),
            "{arch:?}"
        );
    }
}

#[skuld::test]
fn import_base_refuses_a_qcow2_with_a_backing_file(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let overlay = home.join("overlay.qcow2");
    storage
        .create_instance_disk(base.tip().dir(), &overlay, 2 << 20)
        .unwrap();

    let error = world
        .store
        .build(&request(key(8)), None, |staging| {
            storage.import_base(&overlay, Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
}

#[skuld::test]
fn vars_are_copied_per_instance_and_do_not_alias(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let template = std::fs::read(base.tip().dir().path().join(VARS)).unwrap();
    let (first, second) = (home.join("one").join("disk.img"), home.join("two").join("disk.img"));
    for disk in [&first, &second] {
        std::fs::create_dir_all(disk.parent().unwrap()).unwrap();
        storage.create_instance_disk(base.tip().dir(), disk, 4 << 20).unwrap();
    }
    let vars_of = |disk: &Path| disk.parent().unwrap().join(VARS);
    assert_eq!(std::fs::read(vars_of(&first)).unwrap(), template);

    // The guest writes its first instance's variables.
    std::fs::write(vars_of(&first), b"changed by the first guest").unwrap();

    assert_eq!(std::fs::read(vars_of(&second)).unwrap(), template);
    assert_eq!(std::fs::read(base.tip().dir().path().join(VARS)).unwrap(), template);
}
