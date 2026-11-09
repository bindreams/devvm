//! Layer builds against QEMU and a real Linux guest.

use std::path::Path;
use std::sync::Arc;

use crate::backend::qemu::qemu_parts;
use crate::backend::qemu::sidecar::QemuInstall;
use crate::backend::qemu::storage::DISK;
use crate::config::Config;
use crate::engine::engine_tests::engine_with;
use crate::exit::StderrProgress;
use crate::host;
use crate::recipe::Recipe;
use crate::store::fetch::{Fetcher, HttpTransport};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::qemu::{qemu_img_info, qemu_sidecar, scratch_run, write_config};
use crate::{BuildOptions, Dirs, Image};

const DISK_20_GIB: u64 = 20 << 30;

/// Every layer's disk size, root first, as `qemu-img` reads it.
fn virtual_sizes(install: &QemuInstall, image: &Image) -> Vec<u64> {
    image
        .chain
        .layers()
        .iter()
        .map(|layer| {
            let info = qemu_img_info(install, &layer.dir().path().join(DISK), false);
            info["virtual-size"].as_u64().expect("a virtual size")
        })
        .collect()
}

/// A recipe with one step that needs the guest to be up, with the disk size `disk` (`20GiB`) unless it is `None`.
fn recipe(home: &Path, disk: Option<&str>) -> Recipe {
    let resources = disk
        .map(|size| format!("resources:\n  disk: {size}\n"))
        .unwrap_or_default();
    Recipe::parse(
        &format!("from: ubuntu-24.04\n{resources}steps:\n  - run: 'true'\n"),
        &home.join("r.devvm.yaml"),
    )
    .unwrap()
}

/// A build with the default disk and then one with `disk: 20GiB`: the second succeeds, shares no layer with the first, and
/// commits layers of 20 GiB. Each step's instance has the recipe's disk size unchanged, so the layers and the layer
/// keys agree on it.
#[skuld::test]
fn a_default_size_build_then_a_20_gib_build_share_no_layer_and_commit_20_gib_layers(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    // The runtime directory is short, so that a control socket in it fits the host's limit.
    let run = scratch_run();
    let state = ["config", "data", "cache", "state"].map(|name| {
        (
            format!("XDG_{}_HOME", name.to_uppercase()),
            home.join(name).into_os_string(),
        )
    });
    let var = |name: &str| {
        if name == "XDG_RUNTIME_DIR" {
            return Some(run.path().as_os_str().to_owned());
        }
        state
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let dirs = Dirs::from_lookup(Some(home.to_path_buf()), &var, &|| {
        unreachable!("XDG_RUNTIME_DIR is set")
    })
    .unwrap();
    // The QEMU of the fixture, used in place.
    write_config(
        &dirs,
        &Config {
            qemu_path: Some(install.bin.clone()),
        },
    );
    let progress = Arc::new(StderrProgress);
    let transport = HttpTransport::new().expect("an HTTP client");
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), Arc::new(transport), progress.clone()));
    let parts = qemu_parts(&dirs, fetcher, host::current(), progress.clone());
    let engine = engine_with(dirs, host::current(), Some(parts.backend), parts.guests, progress);

    let default = engine.build(&recipe(home, None), &BuildOptions::default()).unwrap();
    let bigger = engine
        .build(&recipe(home, Some("20GiB")), &BuildOptions::default())
        .unwrap();

    let keys = |image: &Image| -> Vec<_> { image.chain.refs().iter().map(|layer| layer.key).collect() };
    let (default_keys, bigger_keys) = (keys(&default), keys(&bigger));
    assert!(
        default_keys.iter().all(|key| !bigger_keys.contains(key)),
        "the builds share a layer: {default_keys:?} {bigger_keys:?}"
    );
    let sizes = virtual_sizes(install, &bigger);
    assert_eq!(sizes.len(), 2, "the base and one step");
    assert_eq!(
        sizes[1], DISK_20_GIB,
        "the step's layer is its instance's disk, unchanged: {sizes:?}"
    );
}
