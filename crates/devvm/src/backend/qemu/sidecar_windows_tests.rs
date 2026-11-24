use std::path::Path;

use super::sidecar_tests::{entries, files, visible, world, world_pinning_another_build, zip_of};
use crate::contain::Lifetime;
use crate::host;
use crate::test_support::fake_vm;
use crate::test_support::fixtures::unicode_home;

/// The error a rename fails with while a file inside the directory is open without delete sharing, measured on Windows
/// 11 (build 26100) on 2026-11-24: `ERROR_ACCESS_DENIED`, which the host names as the sign of an install in use.
#[skuld::test]
fn renaming_a_directory_with_a_held_file_fails_with_the_code_the_host_names(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("held");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin").join("tool"), b"tool").unwrap();
    let held = host::current().open_held_read(&dir.join("bin").join("tool")).unwrap();

    let refused = std::fs::rename(&dir, home.join("moved")).unwrap_err();

    assert_eq!(
        refused.raw_os_error(),
        host::current().rename_refused_by_open_file(),
        "{refused:?}"
    );
    drop(held);
    std::fs::rename(&dir, home.join("moved")).unwrap();
}

/// Two devvm builds with different QEMU pins share one cache. Each has resolved its install and not yet launched from
/// it: the value it holds is all that says the install is in use.
#[skuld::test]
fn an_install_a_value_holds_survives_the_other_pins_resolves_and_goes_when_dropped(
    #[fixture(unicode_home)] home: &Path,
) {
    let first = world(home, zip_of(&files()));
    let second = world_pinning_another_build(home);
    let qemu = first.dirs.qemu();

    let resolved_first = first.source.resolve().unwrap();
    let resolved_second = second.source.resolve().unwrap();

    assert_eq!(visible(&qemu).len(), 2, "{:?}", entries(&qemu));
    for install in [&resolved_first, &resolved_second] {
        assert!(install.tool("qemu-img").is_file());
        assert!(install.share.is_dir());
    }
    // The first build resolves again; the second's install is held too.
    first.source.resolve().unwrap();
    assert_eq!(visible(&qemu).len(), 2, "{:?}", entries(&qemu));
    assert!(resolved_second.tool("qemu-img").is_file());

    drop(resolved_first);
    second.source.resolve().unwrap();

    assert_eq!(visible(&qemu).len(), 1, "{:?}", entries(&qemu));
    assert!(resolved_second.tool("qemu-img").is_file());
    let all = entries(&qemu);
    assert!(!all.iter().any(|name| name.starts_with(".extract-")), "{all:?}");
    assert!(first.progress.warnings().is_empty(), "{:?}", first.progress.warnings());
    assert!(
        second.progress.warnings().is_empty(),
        "{:?}",
        second.progress.warnings()
    );
}

/// The install `existing` returns holds too.
#[skuld::test]
fn the_install_that_existing_returns_is_held(#[fixture(unicode_home)] home: &Path) {
    let first = world(home, zip_of(&files()));
    let second = world_pinning_another_build(home);
    first.source.resolve().unwrap();
    let existing = first.source.existing().unwrap().unwrap();

    second.source.resolve().unwrap();

    assert!(existing.tool("qemu-img").is_file(), "{:?}", entries(&first.dirs.qemu()));
    assert_eq!(visible(&first.dirs.qemu()).len(), 2);
}

/// A process that holds a duplicate of the install's hold keeps the install whole until it exits, whatever became of
/// the value that was resolved. The fake VM, started frozen as QEMU is, stands in for QEMU; the process is spawned, so
/// this runs in CI only.
#[skuld::test]
fn an_install_whose_hold_was_given_to_a_process_stays_whole_until_that_process_exits(
    #[fixture(unicode_home)] home: &Path,
) {
    let first = world(home, zip_of(&files()));
    let second = world_pinning_another_build(home);
    let qemu = first.dirs.qemu();
    let mut vm = fake_vm::frozen(home, Lifetime::DiesWithOwner);
    let installed = {
        let install = first.source.resolve().unwrap();
        host::current()
            .share_open_file_with(install.held.as_ref().expect("a downloaded install is held"), vm.id())
            .unwrap();
        install.bin.parent().unwrap().to_path_buf()
    };

    // Nothing in this process holds the install now; the process it was given to does.
    second.source.resolve().unwrap();

    for (file, _) in files() {
        assert!(
            installed.join(&file).is_file(),
            "{file} is gone from the install in use"
        );
    }
    assert_eq!(visible(&qemu).len(), 2, "{:?}", entries(&qemu));
    vm.kill().unwrap();

    second.source.resolve().unwrap();
    assert!(!installed.exists(), "{:?}", entries(&qemu));
}
