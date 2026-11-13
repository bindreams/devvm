//! The qcow2 storage against the sidecar's `qemu-img`. All of it runs under the Unicode test directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::qemu::sidecar::QemuInstall;
use crate::backend::qemu::storage::{DISK, QCOW2, QemuStorage, VARS};
use crate::backend::{DEFAULT_DISK, Patch, QCOW2_MAX_SIZE, Storage};
use crate::host;
use crate::store::layers::layers_tests::{Env, env, key, request};
use crate::store::layers::{Built, ChainLease};
use crate::test_support::fixtures::{UNICODE_DIR, unicode_home};
use crate::test_support::qemu::{
    lazy_for, qemu_img, qemu_img_info, qemu_sidecar, read_through, refused_without_a_disk,
};
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

/// A root layer built under `key(seed)` by importing `source`.
fn layer_from(world: &Env, storage: &QemuStorage, seed: u8, source: &Path) -> ChainLease {
    world
        .store
        .build(&request(key(seed)), None, |staging| {
            storage.import_base(source, Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .unwrap()
}

/// A root layer imported from a raw image of `pattern(BASE_LEN, 1)`.
fn base_layer(world: &Env, storage: &QemuStorage, home: &Path) -> ChainLease {
    layer_from(world, storage, 1, &write_raw(home, "base.raw", &pattern(BASE_LEN, 1)))
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
        .create_instance_disk(base.tip().dir(), &instance, Some(8 << 20))
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
        .create_instance_disk(base.tip().dir(), &instance, Some(64 << 20))
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
            .create_instance_disk(tip.tip().dir(), &instance, Some(8 << 20))
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

/// `qemu-img create -b` would accept such a size and the guest would see a truncated disk. `BASE_LEN - 512` is the
/// largest size that still rounds below the layer.
#[skuld::test]
fn a_disk_smaller_than_its_layer_is_refused_naming_both_sizes_and_creates_nothing(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);

    for size in [0, BASE_LEN as u64 / 2, BASE_LEN as u64 - 512] {
        let error = refused_without_a_disk(&storage, &base, home, Some(size), ErrorKind::Recipe);

        assert!(error.message().contains(&size.to_string()), "{size}: {error}");
        assert!(error.message().contains("resources.disk"), "{size}: {error}");
        assert!(!error.message().contains("  "), "{size}: {error}");
        assert!(error.message().contains(&BASE_LEN.to_string()), "{size}: {error}");
        assert!(
            error.message().contains(base.tip().dir().path().to_str().unwrap()),
            "{size}: {error}"
        );
    }
}

/// A layer's `disk.qcow2` is read as qcow2, never probed: raw bytes are an error, not a disk of their length. The size
/// is below those bytes' length, so a raw probe would refuse it as `Recipe` instead.
#[skuld::test]
fn a_layer_disk_that_is_not_qcow2_has_no_virtual_size(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let raw = world
        .store
        .build(&request(key(5)), None, |staging| {
            std::fs::write(staging.dir.path().join(DISK), pattern(BASE_LEN, 1)).unwrap();
            std::fs::write(staging.dir.path().join(VARS), b"vars").unwrap();
            Ok(Built::default())
        })
        .unwrap();
    let instance = home.join("raw").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    let error = storage
        .create_instance_disk(raw.tip().dir(), &instance, Some(512))
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("qemu-img info"), "{error}");
    assert!(
        matches!(host::symlink_metadata_if_present(&instance), Ok(None)),
        "a disk was made over a layer that is not qcow2"
    );
}

#[skuld::test]
fn a_disk_equal_to_its_layer_is_made(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let instance = home.join("exact").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    storage
        .create_instance_disk(base.tip().dir(), &instance, Some(BASE_LEN as u64))
        .unwrap();

    assert_eq!(qemu_img_info(install, &instance, false)["virtual-size"], BASE_LEN);
    let vars = host::symlink_metadata_if_present(&instance.parent().unwrap().join(VARS)).unwrap();
    assert!(
        vars.is_some_and(|meta| meta.is_file()),
        "the firmware variables were not copied beside the disk"
    );
}

/// The ceiling itself is made: qemu's own limit (2 PiB at 64 KiB clusters) is where the refusal starts, not below it.
#[skuld::test]
fn a_disk_of_the_largest_qcow2_size_is_made(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let instance = home.join("largest").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    storage
        .create_instance_disk(base.tip().dir(), &instance, Some(1 << 51))
        .unwrap();

    assert_eq!(qemu_img_info(install, &instance, false)["virtual-size"], 1u64 << 51);
}

#[skuld::test]
fn a_disk_that_rounds_up_to_its_layer_is_made(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let instance = home.join("rounded").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    storage
        .create_instance_disk(base.tip().dir(), &instance, Some(BASE_LEN as u64 - 1))
        .unwrap();

    assert_eq!(qemu_img_info(install, &instance, false)["virtual-size"], BASE_LEN);
}

#[skuld::test]
fn a_disk_with_no_size_is_the_default_when_the_layer_is_smaller(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let base = base_layer(&world, &storage, home);
    let instance = home.join("default").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    storage.create_instance_disk(base.tip().dir(), &instance, None).unwrap();

    assert_eq!(qemu_img_info(install, &instance, false)["virtual-size"], DEFAULT_DISK);
}

/// The default is a floor nobody chose, so it never refuses a layer bigger than itself.
#[skuld::test]
fn a_disk_with_no_size_is_the_layers_when_the_layer_is_over_the_default(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let layer_size = DEFAULT_DISK + (1 << 30);
    let source = home.join("huge.qcow2");
    qemu_img(
        install,
        &[
            "create".as_ref(),
            "-f".as_ref(),
            QCOW2.as_ref(),
            source.as_os_str(),
            layer_size.to_string().as_ref(),
        ],
    );
    let huge = layer_from(&world, &storage, 2, &source);
    let instance = home.join("huge").join("disk.img");
    std::fs::create_dir_all(instance.parent().unwrap()).unwrap();

    storage.create_instance_disk(huge.tip().dir(), &instance, None).unwrap();

    assert_eq!(qemu_img_info(install, &instance, false)["virtual-size"], layer_size);
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

/// A qcow2 base of `size` bytes with 2 MiB clusters, whose limit is far above the 64 KiB clusters of an instance disk.
fn big_cluster_base(install: &QemuInstall, home: &Path, name: &str, size: u64) -> PathBuf {
    let source = home.join(name);
    qemu_img(
        install,
        &[
            "create".as_ref(),
            "-f".as_ref(),
            QCOW2.as_ref(),
            "-o".as_ref(),
            "cluster_size=2M".as_ref(),
            source.as_os_str(),
            size.to_string().as_ref(),
        ],
    );
    source
}

/// An instance disk is a qcow2 overlay with 64 KiB clusters, so it cannot cover a base bigger than 2 PiB.
#[skuld::test]
fn import_base_refuses_a_base_no_instance_disk_could_cover(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let source = big_cluster_base(install, home, "over.qcow2", (1 << 51) + 1);

    world
        .store
        .build(&request(key(6)), None, |staging| {
            let error = storage.import_base(&source, Arch::Amd64, staging.dir).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Recipe, "{error}");
            assert!(error.message().contains(source.to_str().unwrap()), "{error}");
            assert!(error.message().contains(&(1u64 << 51).to_string()), "{error}");
            assert!(
                matches!(
                    host::symlink_metadata_if_present(&staging.dir.path().join(DISK)),
                    Ok(None)
                ),
                "a base that is refused was copied"
            );
            Ok(Built::default())
        })
        .unwrap();
}

/// A layer imported before `import_base` refused such a base is refused for every request, even a size that would
/// otherwise be refused for being smaller than the layer.
#[skuld::test]
fn a_disk_over_a_layer_above_the_ceiling_is_refused_for_every_size(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let layer_size = (1u64 << 51) + 512;
    let source = big_cluster_base(install, home, "over.qcow2", layer_size);
    let over = world
        .store
        .build(&request(key(8)), None, |staging| {
            std::fs::copy(&source, staging.dir.path().join(DISK)).unwrap();
            std::fs::write(staging.dir.path().join(VARS), b"vars").unwrap();
            Ok(Built::default())
        })
        .unwrap();

    for size in [None, Some(1 << 51)] {
        let error = refused_without_a_disk(&storage, &over, home, size, ErrorKind::Store);

        assert!(error.message().contains(&layer_size.to_string()), "{size:?}: {error}");
        assert!(
            error.message().contains(over.tip().dir().path().to_str().unwrap()),
            "{size:?}: {error}"
        );
        assert!(error.message().contains(&(1u64 << 51).to_string()), "{size:?}: {error}");
        assert!(
            error.message().contains("from a base within that limit"),
            "{size:?}: {error}"
        );
        assert!(!error.message().contains("resources.disk"), "{size:?}: {error}");
        assert!(!error.message().contains("  "), "{size:?}: {error}");
    }
}

#[skuld::test]
fn import_base_takes_a_base_of_the_largest_size_an_instance_disk_covers(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let source = big_cluster_base(install, home, "exact.qcow2", 1 << 51);

    let layer = layer_from(&world, &storage, 7, &source);

    assert_eq!(
        qemu_img_info(install, &disk_of(&layer), false)["virtual-size"],
        1u64 << 51
    );
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
        .create_instance_disk(base.tip().dir(), &overlay, Some(2 << 20))
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
        storage
            .create_instance_disk(base.tip().dir(), disk, Some(4 << 20))
            .unwrap();
    }
    let vars_of = |disk: &Path| disk.parent().unwrap().join(VARS);
    assert_eq!(std::fs::read(vars_of(&first)).unwrap(), template);

    // The guest writes its first instance's variables.
    std::fs::write(vars_of(&first), b"changed by the first guest").unwrap();

    assert_eq!(std::fs::read(vars_of(&second)).unwrap(), template);
    assert_eq!(std::fs::read(base.tip().dir().path().join(VARS)).unwrap(), template);
}

#[skuld::test]
fn a_blank_disk_has_the_virtual_size_and_a_vars_copy(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);
    let size = 8 << 20;

    let layer = world
        .store
        .build(&request(key(1)), None, |staging| {
            storage.create_blank_disk(size, Arch::Amd64, staging.dir)?;
            Ok(Built::default())
        })
        .unwrap();

    let info = qemu_img_info(install, &disk_of(&layer), false);
    assert_eq!(info["format"], "qcow2");
    assert_eq!(info["virtual-size"], size);
    assert!(info.get("backing-filename").is_none(), "{info}");
    assert_eq!(
        std::fs::read(layer.tip().dir().path().join(VARS)).unwrap(),
        std::fs::read(install.share.join("edk2-i386-vars.fd")).unwrap()
    );
    assert!(
        read_through(install, &disk_of(&layer), home)
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[skuld::test]
fn the_real_storage_accepts_what_the_storage_contract_allows(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    crate::backend::contract::storage_accepts_what_its_contract_allows(&storage(install), home);
}

#[skuld::test]
fn a_blank_disk_of_the_largest_size_or_an_unaligned_one_has_the_size_qemu_img_makes(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let world = env(home);
    let storage = storage(install);

    // The largest qcow2 disk, and one byte over a sector, which is made a sector larger.
    for (n, (size, made)) in [(QCOW2_MAX_SIZE, QCOW2_MAX_SIZE), ((1 << 20) + 1, (1 << 20) + 512)]
        .into_iter()
        .enumerate()
    {
        let layer = world
            .store
            .build(&request(key(n as u8 + 1)), None, |staging| {
                storage.create_blank_disk(size, Arch::Amd64, staging.dir)?;
                Ok(Built::default())
            })
            .unwrap();

        assert_eq!(
            qemu_img_info(install, &disk_of(&layer), false)["virtual-size"],
            made,
            "{size}"
        );
    }
}

/// `base` with `patch` laid over it at `offset`.
fn patched(base: &[u8], offset: usize, patch: &[u8]) -> Vec<u8> {
    let mut bytes = base.to_vec();
    bytes[offset..offset + patch.len()].copy_from_slice(patch);
    bytes
}

#[skuld::test]
fn the_overlay_reads_the_patch_where_the_patch_is_and_the_iso_elsewhere(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let storage = storage(install);
    let base = pattern(BASE_LEN, 1);
    let backing = write_raw(home, "backing.iso", &base);
    let (first, second) = (pattern(70_144, 9), pattern(1536, 4));
    // Sector-aligned, as QEMU's raw driver needs, yet neither patch starts or ends on a cluster of the overlay.
    let patches = [
        Patch {
            offset: 102_400,
            bytes: first.clone(),
        },
        Patch {
            offset: 900_096,
            bytes: second.clone(),
        },
    ];
    let into = home.join("overlay.qcow2");

    storage.overlay_with_patches(&backing, &patches, &into).unwrap();

    let info = qemu_img_info(install, &into, false);
    assert_eq!(info["format"], "qcow2");
    assert_eq!(info["virtual-size"], BASE_LEN);
    assert!(info["actual-size"].as_u64().unwrap() < BASE_LEN as u64, "{info}");
    let expected = patched(&patched(&base, 102_400, &first), 900_096, &second);
    assert_eq!(read_through(install, &into, home), expected);
    assert_eq!(std::fs::read(&backing).unwrap(), base, "the backing file is only read");
    let left: Vec<_> = std::fs::read_dir(home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        !left.iter().any(|name| {
            let name = name.to_string_lossy();
            name.contains("patch") || name.starts_with("overlay-")
        }),
        "a patch or staging file was left behind: {left:?}"
    );
}

#[skuld::test]
fn two_calls_for_one_overlay_make_it_once_and_neither_touches_the_other(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let storage = storage(install);
    let base = pattern(BASE_LEN, 3);
    let backing = write_raw(home, "backing.iso", &base);
    let into = home.join("overlay.qcow2");
    let calls = [
        Patch {
            offset: 4096,
            bytes: pattern(4096, 5),
        },
        Patch {
            offset: 8192,
            bytes: pattern(4096, 6),
        },
    ];

    let start_together = std::sync::Barrier::new(calls.len());
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = calls
            .iter()
            .map(|patch| {
                scope.spawn(|| {
                    start_together.wait();
                    storage.overlay_with_patches(&backing, std::slice::from_ref(patch), &into)
                })
            })
            .collect();
        handles.into_iter().map(|handle| handle.join().unwrap()).collect()
    });

    // Whichever call published first has the overlay; the other is refused.
    let winners: Vec<usize> = (0..calls.len()).filter(|n| outcomes[*n].is_ok()).collect();
    assert_eq!(winners.len(), 1, "{outcomes:?}");
    for (n, outcome) in outcomes.iter().enumerate() {
        if n != winners[0] {
            assert_eq!(
                outcome.as_ref().unwrap_err().kind(),
                ErrorKind::AlreadyExists,
                "{outcomes:?}"
            );
        }
    }
    let winner = &calls[winners[0]];
    assert_eq!(
        read_through(install, &into, home),
        patched(&base, winner.offset as usize, &winner.bytes)
    );
    let left: Vec<_> = std::fs::read_dir(home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        !left.iter().any(|name| name.to_string_lossy().starts_with("overlay-")),
        "a staging file was left behind: {left:?}"
    );
}

#[skuld::test]
fn an_iso_path_with_space_cyrillic_and_a_surrogate_pair_backs_an_overlay(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let storage = storage(install);
    let dir = home.join("пробел space 😀, dir");
    std::fs::create_dir_all(&dir).unwrap();
    let base = pattern(BASE_LEN, 2);
    let backing = write_raw(&dir, "win 😀.iso", &base);
    let patch = pattern(4096, 6);
    let into = dir.join("overlay,with comma.qcow2");

    storage
        .overlay_with_patches(
            &backing,
            &[Patch {
                offset: 8192,
                bytes: patch.clone(),
            }],
            &into,
        )
        .unwrap();

    assert_eq!(read_through(install, &into, home), patched(&base, 8192, &patch));
}
