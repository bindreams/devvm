//! Restarting an instance every way there is, and what the first command and the boot id say after each. Each test boots
//! an instance of its own, because it restarts it.

use std::sync::Arc;

use super::bounds::BOOT_BOUND;
use super::{WindowsBase, elevated_facts, facts, ran, user_facts, windows_base, windows_options};
use crate::engine::Engine;
use crate::engine::keys::{read_record, running_of};
use crate::engine::ready::reconnect_after_reset;
use crate::guest::GuestOs;
use crate::guest::windows::WindowsGuest;
use crate::vm_suite::fixture::Up;
use crate::vm_suite::waits::{awaiting, step};
use crate::{Command, ErrorKind, ExitStatus, Guest, Instance, RunAs};

/// How an instance is restarted.
#[derive(Clone, Copy, Debug)]
pub(super) enum Restart {
    /// `stop`, then `start`: a new QEMU process.
    StopStart,
    /// The guest restarts itself (`shutdown /r`) inside the same QEMU process.
    InPlace,
    /// QEMU resets the guest (`Guest::reboot`): a power cycle without a shutdown.
    SystemReset,
}

pub(super) const RESTARTS: [Restart; 3] = [Restart::StopStart, Restart::InPlace, Restart::SystemReset];

/// The guest's boot id, which the boot task writes anew at every boot.
pub(super) fn boot_id(guest: &dyn Guest) -> String {
    let output = ran(guest, "the boot id", &WindowsGuest.boot_id_command()).unwrap();
    assert_eq!(output.status, ExitStatus::Code(0), "{}", output.stderr_lossy());
    let id = output.stdout_lossy().trim().to_owned();
    assert!(!id.is_empty(), "the guest has no boot id");
    id
}

/// Restarts `up` the way `kind` says and returns once the guest is ready again.
pub(super) fn restart(engine: &Arc<Engine>, up: &Up, kind: Restart) {
    let _watch = awaiting(format!("the restart by {kind:?}"), BOOT_BOUND);
    match kind {
        Restart::StopStart => {
            up.stop().unwrap();
            up.start().unwrap();
        }
        Restart::InPlace => restart_in_place(engine, up),
        Restart::SystemReset => up.reboot().unwrap(),
    }
}

/// What ssh-broker prints, with status 255, when its agent ends before it answers a command (its README documents it).
const AGENT_ENDED: &str = "the agent ended before it answered";

/// The guest restarts itself, and the wait is the one that follows a reset: it ends when the guest says another boot id
/// and is ready in the user's session.
fn restart_in_place(engine: &Arc<Engine>, instance: &Instance) {
    let old = boot_id(instance);
    let restart = Command::new(["shutdown", "/r", "/f", "/t", "0"]);
    match ran(instance, "shutdown /r", &restart) {
        // The agent goes down under the command and ssh-broker says so with 255: the guest is going down.
        Ok(output) if output.status == ExitStatus::Code(255) && output.stderr_lossy().contains(AGENT_ENDED) => {}
        Ok(output) => {
            assert_eq!(
                output.status,
                ExitStatus::Code(0),
                "shutdown /r: {}",
                output.stderr_lossy()
            );
            // The status stands when the guest goes down after it: the loss says so, and any other loss is a failure.
            if let Some(loss) = output.loss {
                assert_eq!(loss.kind(), ErrorKind::ConnectionLost, "shutdown /r: {loss}");
            }
        }
        // The guest goes down under the command: the connection drops, or the channel closes before a status.
        Err(e) if matches!(e.kind(), ErrorKind::ConnectionLost | ErrorKind::NoExitStatus) => {}
        Err(e) => panic!("cannot restart the guest in place: {e}"),
    }
    let dir = instance.dir();
    let op = dir.op_shared().unwrap();
    let record = read_record(engine, dir, &op).unwrap();
    let running = running_of(engine, dir, &record, &op).unwrap();
    reconnect_after_reset(
        engine,
        &WindowsGuest,
        dir.name(),
        &running.target,
        &running.record,
        &old,
    )
    .unwrap();
}

/// A new instance called `name`, `check` on its first boot and again after each restart, with the way it began named.
fn across_restarts(base: &WindowsBase, name: &str, mut check: impl FnMut(&Up, &str)) {
    step(&format!("booting {name}"));
    let up = Up::new(&base.image, name, &windows_options()).unwrap();
    check(&up, "first boot");
    for kind in RESTARTS {
        restart(&base.engine, &up, kind);
        step(&format!("checking after {kind:?}"));
        check(&up, &format!("{kind:?}"));
    }
}

#[skuld::test]
fn windows_the_first_command_after_start_stop_start_and_reboot_is_in_session_1(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    across_restarts(base, "w-first", |up, after| {
        assert_eq!(facts(&**up, RunAs::User), user_facts(), "after {after}");
    });
}

#[skuld::test]
fn windows_the_first_elevated_command_after_each_restart_kind_runs_at_high(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    across_restarts(base, "w-elevated", |up, after| {
        assert_eq!(facts(&**up, RunAs::Elevated), elevated_facts(), "after {after}");
    });
}

#[skuld::test]
fn windows_the_boot_id_is_the_same_within_a_boot_and_differs_after_each_restart_kind(
    #[fixture(windows_base)] base: &WindowsBase,
) {
    step("booting an instance");
    let up = Up::new(&base.image, "w-boot-id", &windows_options()).unwrap();
    let mut previous = boot_id(&*up);
    assert_eq!(boot_id(&*up), previous, "two reads in one boot");
    for kind in RESTARTS {
        restart(&base.engine, &up, kind);
        step(&format!("reading the boot id after {kind:?}"));
        let now = boot_id(&*up);
        assert_ne!(now, previous, "the boot id after {kind:?}");
        assert_eq!(boot_id(&*up), now, "two reads in the boot after {kind:?}");
        previous = now;
    }
}
