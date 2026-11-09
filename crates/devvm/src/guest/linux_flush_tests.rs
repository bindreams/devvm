//! The Linux flush script, run by `sh` against a mounts file and a stand-in `sync` that logs its arguments. They spawn
//! `sh`, so they run in the deputy's runner and CI.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use crate::RunAs;
use crate::guest::GuestOs;
use crate::guest::linux::LinuxGuest;
use crate::test_support::fixtures::unicode_home;

const MOUNTS: &str = "\
/dev/vda1 / ext4 rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/vdb1 /mnt/with\\040space ext4 rw 0 0
/dev/vdc /a\\134b ext4 rw 0 0
overlay /merged overlay rw 0 0
/dev/loop0 /snap/x squashfs ro 0 0
/dev/vdd /new\\012line ext4 rw 0 0
/dev/vde /tab\\011and\\0401 ext4 rw 0 0
/dev/vdf /trailing\\012 ext4 rw 0 0
";

/// Runs the flush script over `mounts` with a `sync` that exits `sync_status`; the exit code and the arguments of each
/// call, one call per entry.
fn flush(home: &Path, mounts: &str, sync_status: i32) -> (i32, Vec<Vec<String>>) {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = home.join("sync.log");
    let sync = bin.join("sync");
    std::fs::write(
        &sync,
        format!(
            "#!/bin/sh\necho call >> '{}'\nfor a; do printf '%s\\n' \"$a\" >> '{}'; done\nexit {sync_status}\n",
            log.display(),
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&sync, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mounts_file = home.join("mounts");
    std::fs::write(&mounts_file, mounts).unwrap();
    let command = LinuxGuest.flush_command();
    let path = std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();

    // `$0` is the script's name and `$1` the mounts file it reads in place of `/proc/mounts`.
    let status = std::process::Command::new(&command.argv[0])
        .args(&command.argv[1..])
        .args(["devvm-flush", mounts_file.to_str().unwrap()])
        .env("PATH", path)
        .status()
        .unwrap();

    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    let mut calls: Vec<Vec<String>> = Vec::new();
    for line in logged.split_inclusive('\n') {
        let line = line.strip_suffix('\n').unwrap_or(line);
        match line {
            "call" => calls.push(Vec::new()),
            _ => calls.last_mut().unwrap().push(line.to_owned()),
        }
    }
    (status.code().unwrap(), calls)
}

#[skuld::test]
fn the_flush_runs_as_the_user_so_a_recipe_that_removed_sudo_does_not_break_it() {
    assert_eq!(LinuxGuest.flush_command().run_as, RunAs::User);
}

#[skuld::test]
fn the_flush_syncs_everything_first_then_the_filesystem_of_every_block_device_mount(
    #[fixture(unicode_home)] home: &Path,
) {
    let (code, calls) = flush(home, MOUNTS, 0);

    assert_eq!(code, 0);
    let targets: Vec<String> = calls.iter().map(|call| call.join("|")).collect();
    // A plain `sync` comes first, for what `sync -f` leaves out.
    // `\040`, `\134`, `\012` and `\011` are decoded; a line break inside a name is kept, trailing or not.
    assert_eq!(
        targets,
        [
            "",
            "-f|--|/",
            "-f|--|/mnt/with space",
            "-f|--|/a\\b",
            "-f|--|/snap/x",
            "-f|--|/new|line",
            "-f|--|/tab\tand 1",
            "-f|--|/trailing|",
        ],
    );
}

#[skuld::test]
fn a_failing_sync_fails_the_flush_after_every_mount_was_tried(#[fixture(unicode_home)] home: &Path) {
    let (code, calls) = flush(home, MOUNTS, 1);

    assert_eq!(code, 1);
    assert_eq!(calls.len(), 8, "{calls:?}");
}

#[skuld::test]
fn a_guest_with_no_block_device_mount_only_syncs_plainly_and_succeeds(#[fixture(unicode_home)] home: &Path) {
    let (code, calls) = flush(home, "tmpfs /run tmpfs rw 0 0\nproc /proc proc rw 0 0\n", 0);

    assert_eq!(code, 0);
    // Only the plain `sync`.
    assert_eq!(calls, [Vec::<String>::new()]);
}
