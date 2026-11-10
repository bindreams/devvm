//! Layer builds against QEMU and a real Linux guest.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::backend::files_in_run;
use super::fixture::{Launches, VmEngine, engine_in, guest_options, recipe_in, run_ok, run_ok_as, vm_engine};
use crate::backend::qemu::sidecar::QemuInstall;
use crate::backend::qemu::storage::DISK;
use crate::copy::copy_tests::{add_all, git, new_repo, put};
use crate::recipe::Recipe;
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::qemu::{qemu_img_info, qemu_sidecar, scratch_run};
use crate::{BuildOptions, Command, ExitStatus, Guest, Image, RunAs};

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
    #[fixture(vm_engine)] vm: &VmEngine,
    #[fixture(unicode_home)] home: &Path,
) {
    let engine = &vm.engine;

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

// Recipes in a real guest =============================================================================================

/// A git repository `home/proj` with two tracked files and one that is not tracked.
fn project(home: &Path) -> PathBuf {
    let repo = new_repo(home, "proj");
    put(&repo, "tracked.txt", b"tracked one");
    put(&repo, "dir/with space \u{416}.txt", b"nested");
    add_all(&repo);
    git(&repo, &["commit", "-q", "-m", "tracked"]);
    put(&repo, "untracked.txt", b"not in git");
    repo
}

#[skuld::test]
fn a_recipe_with_run_and_copy_git_tracked_builds_and_an_untracked_file_never_appears(
    #[fixture(vm_engine)] vm: &VmEngine,
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = project(home);
    let recipe = recipe_in(
        &repo,
        &[
            "run: echo ran > /var/tmp/ran.txt",
            "copy: { from: git-tracked, to: /var/tmp/proj }",
        ],
    );

    let image = vm.engine.build(&recipe, &BuildOptions::default()).unwrap();

    let guest = image.start(&guest_options()).unwrap();
    assert_eq!(run_ok(&guest, &["cat", "/var/tmp/ran.txt"]), "ran\n");
    assert_eq!(run_ok(&guest, &["cat", "/var/tmp/proj/tracked.txt"]), "tracked one");
    assert_eq!(
        run_ok(&guest, &["cat", "/var/tmp/proj/dir/with space \u{416}.txt"]),
        "nested"
    );
    // `test -e` exits 1 for a file that is not there.
    let untracked = guest
        .run(&Command::new(["test", "-e", "/var/tmp/proj/untracked.txt"]))
        .unwrap();
    assert_eq!(untracked.status, ExitStatus::Code(1), "{}", untracked.stderr_lossy());
}

/// The step uploads a tar archive to `/tmp` and extracts it. The guest that this test boots has booted again since, and
/// Ubuntu clears `/tmp` at boot, so the archive is not seen here whether or not the step removed it; this checks that
/// no other staging file is left anywhere and that the destination holds exactly the tracked files.
#[skuld::test]
fn a_copy_step_leaves_no_staging_file_outside_tmp_and_exactly_the_tracked_files_at_its_destination(
    #[fixture(vm_engine)] vm: &VmEngine,
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = project(home);
    let recipe = recipe_in(&repo, &["copy: { from: git-tracked, to: /var/tmp/dest }"]);

    let image = vm.engine.build(&recipe, &BuildOptions::default()).unwrap();

    let guest = image.start(&guest_options()).unwrap();
    // As root: `find` exits 1 on a directory that the user cannot read, such as `/root`.
    let left = run_ok_as(
        &guest,
        RunAs::Elevated,
        &[
            "find",
            "/",
            "-xdev",
            "(",
            "-name",
            "devvm-copy-*",
            "-o",
            "-name",
            ".devvm-partial-*",
            ")",
        ],
    );
    assert_eq!(left, "", "files a copy step left");
    let dest = run_ok(&guest, &["find", "/var/tmp/dest", "-mindepth", "1"]);
    let mut found: Vec<&str> = dest.lines().collect();
    found.sort();
    assert_eq!(
        found,
        [
            "/var/tmp/dest/dir",
            "/var/tmp/dest/dir/with space \u{416}.txt",
            "/var/tmp/dest/tracked.txt"
        ]
    );
}

/// The layers before a `copy` step are keyed by what they run, so a file that changes only the copy's files changes only
/// the copy's layer, and the steps before it run in no guest again.
#[skuld::test]
fn vm_touching_only_a_tracked_file_reuses_every_layer_before_copy(
    #[fixture(vm_engine)] vm: &VmEngine,
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = project(home);
    let recipe = recipe_in(
        &repo,
        &[
            "run: echo one > /var/tmp/one",
            "run: echo two > /var/tmp/two",
            "copy: { from: git-tracked, to: /var/tmp/proj }",
        ],
    );
    let first = vm.engine.build(&recipe, &BuildOptions::default()).unwrap().chain.refs();

    put(&repo, "tracked.txt", b"tracked two");
    let launched = vm.launches.count();
    let second = vm.engine.build(&recipe, &BuildOptions::default()).unwrap().chain.refs();

    assert_eq!(first.len(), 4, "the base and three steps");
    assert_eq!(second[..3], first[..3], "the layers before the copy are rebuilt");
    assert_ne!(
        second[3], first[3],
        "the copy's layer is reused though its file changed"
    );
    assert_eq!(vm.launches.count() - launched, 1, "only the copy step boots a guest");
}

#[skuld::test]
fn vm_a_copy_to_a_tilde_destination_lands_in_the_guest_home(
    #[fixture(vm_engine)] vm: &VmEngine,
    #[fixture(unicode_home)] home: &Path,
) {
    let repo = project(home);
    // The second step fails the build unless `~/x` is there.
    let recipe = recipe_in(
        &repo,
        &["copy: { from: git-tracked, to: ~/x }", "run: cd ~/x && ls tracked.txt"],
    );

    let image = vm.engine.build(&recipe, &BuildOptions::default()).unwrap();

    let guest = image.start(&guest_options()).unwrap();
    let in_home = run_ok(&guest, &["sh", "-c", "test -f \"$HOME/x/tracked.txt\" && echo yes"]);
    assert_eq!(in_home, "yes\n");
}

#[skuld::test]
fn a_second_build_of_an_unchanged_recipe_boots_no_vm(
    #[fixture(vm_engine)] vm: &VmEngine,
    #[fixture(unicode_home)] home: &Path,
) {
    let recipe = recipe_in(home, &["run: echo cached > /var/tmp/cached"]);
    let first = vm.engine.build(&recipe, &BuildOptions::default()).unwrap().chain.refs();

    let launched = vm.launches.count();
    let second = vm.engine.build(&recipe, &BuildOptions::default()).unwrap().chain.refs();

    assert_eq!(second, first);
    assert_eq!(
        vm.launches.count(),
        launched,
        "a guest was booted for a build that was cached"
    );
}

/// A build step's guest leaves no socket, lock or claim file in the run directory, and no directory in `<state>/builds`.
///
/// The engine is this test's own, over the shared engine's stores and a runtime directory that nothing else launches in,
/// so the listing sees only this build.
#[skuld::test]
fn vm_qemu_guest_a_build_step_leaves_no_runtime_files(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(vm_engine)] vm: &VmEngine,
    #[fixture(unicode_home)] home: &Path,
) {
    let scratch = scratch_run();
    let dirs = vm.roots.dirs_with_run(scratch.path()).unwrap();
    let launches = Arc::new(Launches::default());
    let engine = engine_in(&dirs, install, launches.clone());
    let recipe = recipe_in(home, &["run: echo runtime-files > /var/tmp/runtime-files"]);

    engine.build(&recipe, &BuildOptions::default()).unwrap();

    assert_eq!(launches.count(), 1, "the step booted no guest, so this checks nothing");
    let run = dirs.run().unwrap();
    assert_eq!(
        files_in_run(run),
        Vec::<PathBuf>::new(),
        "the run directory {}",
        run.display()
    );
    assert_eq!(
        names(&dirs.builds()),
        Vec::<String>::new(),
        "{}",
        dirs.builds().display()
    );
}
