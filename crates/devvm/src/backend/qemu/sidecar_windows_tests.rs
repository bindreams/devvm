use std::path::Path;

use super::sidecar_tests::{entries, files, old_install, visible, world, world_pinning_another_build, zip_of};
use crate::contain::contain_tests::Sleeper;
use crate::host;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::helper;

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

/// A running executable in place of the held file: a copy of this test binary, installed as `qemu-system-x86_64` and
/// running as the `sleeper` helper.
#[skuld::test]
fn an_older_install_whose_executable_is_running_is_kept_whole_until_it_exits(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let qemu = world.dirs.qemu();
    let running = old_install(&qemu, "10.0.0-0123456789ab");
    let exe = running
        .join("bin")
        .join(format!("qemu-system-x86_64{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(helper::exe(), &exe).unwrap();
    let child = std::process::Command::new(&exe)
        .env(helper::HELPER_VAR, "sleeper")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let sleeper = Sleeper { child };

    world.source.resolve().unwrap();

    for (file, _) in files() {
        assert!(running.join(&file).is_file(), "{file} is gone from the install in use");
    }
    sleeper.finish();

    world.source.resolve().unwrap();
    assert!(!running.exists(), "{:?}", entries(&qemu));
}
