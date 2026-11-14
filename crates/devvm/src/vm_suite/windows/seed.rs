//! The instance's identity: the host key the seed gives it, which survives a stop and a hard reset and differs between
//! instances, and the plain `ssh` that the generated configuration lets in.

use std::net::SocketAddr;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::TryRecvError;

use super::session::ssh_remote;
use super::shared::{ANSWER_BOUND, BOOT_BOUND, step, watch};
use super::{PACE, SharedWindowsGuest, WindowsBase, shared_windows_guest, windows_base, windows_options};
use crate::engine::Engine;
use crate::engine::keys::{read_record, running_of};
use crate::host::{self, is_absent, read_from};
use crate::vm_suite::fixture::Up;

/// What `ssh-keyscan` of the instance's forwarded port says its sshd offers, as `<type> <base64>` for each key.
fn offered_keys(addr: SocketAddr) -> Vec<String> {
    let _watch = watch(format!("ssh-keyscan of {addr}"), ANSWER_BOUND);
    let mut command = std::process::Command::new("ssh-keyscan");
    command
        .args(["-p", &addr.port().to_string(), &addr.ip().to_string()])
        .stdin(Stdio::null());
    host::current().quiet(&mut command);
    let output = command
        .output()
        .unwrap_or_else(|e| panic!("cannot run ssh-keyscan: {e}"));
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields.len() >= 3).then(|| format!("{} {}", fields[1], fields[2]))
        })
        .collect()
}

/// Where the running instance `up` is reached, and the host key devvm pins for it as `<type> <base64>`.
fn pinned(engine: &Arc<Engine>, up: &Up) -> (SocketAddr, String) {
    let dir = up.dir();
    let op = dir.op_shared().unwrap();
    let record = read_record(engine, dir, &op).unwrap();
    let running = running_of(engine, dir, &record, &op).unwrap();
    let fields: Vec<&str> = running.target.host_public.split_whitespace().take(2).collect();
    (running.target.addr, fields.join(" "))
}

/// The one key `up`'s sshd offers, which is the one devvm pins.
fn the_one_key(engine: &Arc<Engine>, up: &Up) -> String {
    let (addr, pinned) = pinned(engine, up);
    assert_eq!(
        offered_keys(addr),
        std::slice::from_ref(&pinned),
        "the keys {addr} offers"
    );
    pinned
}

#[skuld::test]
fn windows_two_instances_have_different_host_keys(#[fixture(windows_base)] base: &WindowsBase) {
    step("booting the first instance");
    let first = Up::new(&base.image, "w-keys-one", &windows_options()).unwrap();
    let one = the_one_key(&base.engine, &first);
    // One guest at a time: two of Windows' size would load a hosted runner.
    step("stopping it and booting the second");
    first.stop().unwrap();
    let second = Up::new(&base.image, "w-keys-two", &windows_options()).unwrap();
    let two = the_one_key(&base.engine, &second);

    assert_ne!(one, two);
}

#[skuld::test]
fn windows_a_stopped_instance_keeps_its_host_key(#[fixture(windows_base)] base: &WindowsBase) {
    step("booting an instance");
    let up = Up::new(&base.image, "w-keeps-key", &windows_options()).unwrap();
    let key = the_one_key(&base.engine, &up);

    step("stopping and starting it");
    up.stop().unwrap();
    up.start().unwrap();

    assert_eq!(the_one_key(&base.engine, &up), key);
}

/// The serial log holds a whole line `marker`, as the boot task writes it.
fn serial_has(log: &Path, marker: &str) -> bool {
    match read_from(log, 0) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .split(['\n', '\r'])
            .any(|line| line.trim_end() == marker),
        Err(e) if is_absent(&e) => false,
        Err(e) => panic!("cannot read {}: {e}", log.display()),
    }
}

/// QEMU resets the guest as soon as the boot task has written the host key, a real condition read from the serial log:
/// the cached writes of the seed are at their most at risk then. The boot task writes the seed again at the next boot,
/// and sshd offers only the seeded ed25519 key.
#[skuld::test]
fn windows_a_hard_reset_right_after_the_seed_write_keeps_the_seeded_host_key(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    step("starting an instance and resetting it at the seed write");
    // The loops below end with the start, which the engine's boot bound ends; this ends a start that stops for good.
    let _watch = watch("the instance starting and resetting", BOOT_BOUND);
    let up = Up::created(&base.image, "w-hard-reset", &windows_options()).unwrap();
    let resumed = base.launches.resumed();
    let backend = base.engine.backend().unwrap();

    let reset = std::thread::scope(|scope| {
        let start = scope.spawn(|| up.start());
        let mut reset = false;
        // The machine's record, once it runs; the start may end first, in a failure the join reports.
        let record = loop {
            match resumed.try_recv() {
                Ok(record) => break Some(record),
                Err(TryRecvError::Empty) if !start.is_finished() => std::thread::sleep(PACE),
                // A record sent after the start ended is never used.
                Err(_) => break None,
            }
        };
        if let Some(record) = record {
            while !start.is_finished() {
                if serial_has(&record.serial_log, "DEVVM-SEED-WRITTEN=ssh_host_ed25519_key") {
                    backend.attach(&record).unwrap().reset().unwrap();
                    reset = true;
                    break;
                }
                std::thread::sleep(PACE);
            }
        }
        start.join().unwrap().unwrap();
        reset
    });

    step("checking the host key after the reset");
    assert!(reset, "the guest was ready before the boot task wrote the host key");
    // The start waited for a guest that accepts the pinned key; sshd offers it alone.
    the_one_key(&base.engine, &up);
}

#[skuld::test]
fn windows_plain_ssh_with_the_generated_config_connects_without_a_prompt(
    #[fixture(shared_windows_guest)] guest: &SharedWindowsGuest,
) {
    let config = guest.vm.instance().ssh_config().unwrap();

    let output = ssh_remote(&config, guest.vm.name(), "Write-Output ok");

    assert!(
        output.status.success(),
        "ssh -F {} {}: {} {}",
        config.display(),
        guest.vm.name(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}
