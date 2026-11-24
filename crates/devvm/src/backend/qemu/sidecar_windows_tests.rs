use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

use super::sidecar_tests::{World, entries, files, old_install, visible, world, zip_of};
use crate::contain::Lifetime;
use crate::host;
use crate::test_support::fake_vm;
use crate::test_support::fixtures::unicode_home;

/// A world over the same cache as `world(home, zip_of(&files()))` whose manifest pins another build: the zip holds one
/// more file, so its hash and the name of its install differ.
fn world_pinning_another_build(home: &Path) -> World {
    let mut other = files();
    other.push(("share/another-build.txt".to_owned(), b"another build".to_vec()));
    world(home, zip_of(&other))
}

/// The error a rename fails with while a file inside the directory is open without delete sharing, measured on Windows
/// 11 (build 26100) on 2026-11-24: `ERROR_ACCESS_DENIED`, which the host names as the sign of an install in use.
#[skuld::test]
fn renaming_a_directory_with_a_held_file_fails_with_the_code_the_host_names(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("held");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::write(dir.join("bin").join("tool"), b"tool").unwrap();
    let held = host::current().open_held_read(&dir.join("bin").join("tool")).unwrap();

    let refused = std::fs::rename(&dir, home.join("moved")).unwrap_err();

    let code = refused.raw_os_error().expect("an operating system error");
    assert!(
        host::current().rename_refused_by_open_file().contains(&code),
        "{refused:?}"
    );
    drop(held);
    std::fs::rename(&dir, home.join("moved")).unwrap();
}

/// `dir` open as the system opens a process's working directory for `SetCurrentDirectory`: `FILE_TRAVERSE` and share
/// mode read and write. Held without making this process's working directory the directory.
fn open_as_working_directory(dir: &Path) -> std::fs::File {
    const FILE_TRAVERSE: u32 = 0x20;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const FILE_SHARE_READ_WRITE: u32 = 0x1 | 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .access_mode(FILE_TRAVERSE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .unwrap()
}

/// Measured on Windows 11 (build 26100) on 2026-11-24: the rename of a directory that is open as a working directory
/// fails with `ERROR_SHARING_VIOLATION` (32), where one with a file open inside fails with `ERROR_ACCESS_DENIED` (5).
#[skuld::test]
fn renaming_a_directory_held_as_a_working_directory_fails_with_a_code_the_host_names(
    #[fixture(unicode_home)] home: &Path,
) {
    let dir = home.join("cwd");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let open = open_as_working_directory(&dir);

    let refused = std::fs::rename(&dir, home.join("moved")).unwrap_err();

    let code = refused.raw_os_error().expect("an operating system error");
    assert!(
        host::current().rename_refused_by_open_file().contains(&code),
        "{refused:?}"
    );
    drop(open);
    std::fs::rename(&dir, home.join("moved")).unwrap();
}

/// A terminal whose working directory is a superseded install keeps it: removal leaves it as it was, and says nothing.
#[skuld::test]
fn an_install_that_is_a_working_directory_is_kept_without_a_warning(#[fixture(unicode_home)] home: &Path) {
    let world = world(home, zip_of(&files()));
    let qemu = world.dirs.qemu();
    let old = old_install(&qemu, "10.0.0-0123456789ab");
    let open = open_as_working_directory(&old);

    world.source.resolve().unwrap();

    assert!(old.join("bin").is_dir(), "{:?}", entries(&qemu));
    assert!(world.progress.warnings().is_empty(), "{:?}", world.progress.warnings());
    drop(open);
    world.source.resolve().unwrap();
    assert!(!old.exists(), "{:?}", entries(&qemu));
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
