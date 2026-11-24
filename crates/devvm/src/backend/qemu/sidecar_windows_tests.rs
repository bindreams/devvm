use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use super::sidecar_tests::{entries, files, old_install, visible, world, zip_of};
use crate::contain::contain_tests::Sleeper;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::helper;

const FILE_SHARE_READ: u32 = 1;

/// Windows cannot delete a running executable. A file held open without delete sharing stands in for one: the install
/// that holds it stays whole, and the install beside it still goes.
#[skuld::test]
fn an_older_install_that_is_in_use_is_kept_whole_and_the_rest_are_removed(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let qemu = world.dirs.qemu();
    let in_use = old_install(&qemu, "10.0.0-0123456789ab");
    let unused = old_install(&qemu, "10.0.1-0123456789ab");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(
            in_use
                .join("bin")
                .join(format!("qemu-system-x86_64{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();

    let install = world.source.resolve().unwrap();

    assert!(install.tool("qemu-img").is_file());
    assert!(!unused.exists(), "{:?}", entries(&qemu));
    for (file, _) in files() {
        assert!(in_use.join(&file).is_file(), "{file} is gone from the install in use");
    }
    assert_eq!(visible(&qemu).len(), 2, "{:?}", entries(&qemu));
    drop(held);

    // Once nothing holds it, the next resolve removes it.
    world.source.resolve().unwrap();
    assert!(!in_use.exists(), "{:?}", entries(&qemu));
}

/// The same, with a running executable in place of the stand-in: a copy of this test binary, installed as
/// `qemu-system-x86_64` and running as the `sleeper` helper.
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
