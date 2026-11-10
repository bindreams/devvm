//! The guest operations of `Instance` over an instance whose machine is this very process, served by the fixture's SSH
//! server: no VM is launched, so these run anywhere.

use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use secrecy::ExposeSecret;

use crate::access::VmExit;
use crate::backend::MachineRecord;
use crate::engine::engine_tests::{
    EngineFixture, engine_fixture, engine_with_bound, instance_in, memory, record_machine, running_machine,
    stub_fixture, watch_with, watched, write_conf,
};
use crate::engine::keys::ensure_keys;
use crate::exit::ExitStatus;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::TestHost;
use crate::test_support::stub_backend::Attach;
use crate::vm::Guest;
use crate::{Command, ErrorKind, Instance};

/// The instance `foo` with its keys, recorded as running as this process with its SSH endpoint at the fixture's server,
/// which trusts the instance's keys.
pub(crate) fn running_instance(fx: &EngineFixture, home: &Path) -> Instance {
    let dir = instance_in(fx, "foo", home);
    let keys = {
        let op = dir.op_exclusive().unwrap();
        ensure_keys(&dir, &op, &TestHost, &mut |_| {}).unwrap()
    };
    fx.server
        .install(&keys.client_public, keys.host_private.expose_secret());
    record_machine(&dir, &running_machine(home, fx.server.addr()));
    Instance::open_in(&fx.engine, "foo").unwrap()
}

fn recorded(instance: &Instance) -> Option<MachineRecord> {
    instance.dir().record().unwrap().machine
}

/// A writer that tells a channel every chunk it is given.
struct Tell(mpsc::Sender<Vec<u8>>);

impl Write for Tell {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        self.0.send(chunk.to_vec()).unwrap();
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A writer whose reader has gone away.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the reader went away",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A writer that takes everything and refuses the final flush.
struct FlushRefused;

impl Write for FlushRefused {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the final flush was refused",
        ))
    }
}

/// Makes the VM of `instance` exit when the returned sender is used.
fn exit_on_demand(fx: &EngineFixture, instance: &Instance) -> mpsc::Sender<()> {
    let (end, ended) = mpsc::channel::<()>();
    watch_with(
        &fx.engine,
        recorded(instance).unwrap().process,
        VmExit::when(move || {
            let _ = ended.recv();
            Ok(())
        })
        .unwrap(),
    );
    end
}

// Run =================================================================================================================

#[skuld::test]
fn run_collects_the_output_and_the_status(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);

    let output = instance.run(&Command::new(["echo", "hello", "there"])).unwrap();
    assert_eq!(output.stdout, b"hello there\n");
    assert_eq!(output.status, ExitStatus::Code(0));

    let output = instance.run(&Command::new(["exit", "3"])).unwrap();
    assert_eq!(output.status, ExitStatus::Code(3));
}

#[skuld::test]
fn run_to_streams_without_buffering(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);
    let (told, heard) = mpsc::channel();
    // The guest command never ends by itself, so a chunk that arrives did so while it ran.
    let end = exit_on_demand(&fx, &instance);

    let mut out = Tell(told);

    let result = std::thread::scope(|scope| {
        let run =
            scope.spawn(|| instance.run_to(&Command::new(["write-once-and-hold"]), &mut out, &mut std::io::sink()));
        assert_eq!(
            heard.recv().unwrap(),
            b"first\n",
            "the line reaches the writer while the command runs"
        );
        end.send(()).unwrap();
        run.join().unwrap()
    });

    let error = result.expect_err("the VM exits under the held command");
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
}

#[skuld::test]
fn run_to_turns_a_closed_writer_into_an_io_error_and_stops_the_guest_command(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);

    let error = instance
        .run_to(&Command::new(["write-forever"]), &mut Closed, &mut std::io::sink())
        .expect_err("nobody reads the output");

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("the reader went away"), "{error}");
    assert!(error.message().contains("stopped"), "{error}");
    // The connection ends once `run_to`'s session and the exec task's clone are gone, and the task drops its clone after
    // queueing the close. `write-forever` returns on the close or on the end of the connection, so its return holds in
    // either order.
    fx.server.wait_for_scripts_to_return(1);
}

#[skuld::test]
fn run_to_names_the_status_when_the_output_closed_after_the_command_had_ended(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);

    let error = instance
        .run_to(&Command::new(["exit", "4"]), &mut FlushRefused, &mut std::io::sink())
        .expect_err("the final flush fails");

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("the final flush was refused"), "{error}");
    assert!(error.message().contains("exit code 4"), "{error}");
    assert!(
        !error.message().contains("stopped"),
        "the command was not stopped: {error}"
    );
}

#[skuld::test]
fn run_to_returns_the_status_and_reports_the_loss_on_progress(#[fixture(unicode_home)] home: &Path) {
    let progress = memory();
    let fx = engine_fixture(home, progress.clone());
    let instance = running_instance(&fx, home);
    let mut out = Vec::new();

    let status = instance
        .run_to(
            &Command::new(["status-then-extended-data", "5"]),
            &mut out,
            &mut std::io::sink(),
        )
        .unwrap();

    assert_eq!(status, ExitStatus::Code(7));
    assert_eq!(out, b"before the status\n");
    let messages = progress.messages();
    assert!(
        messages.iter().any(|m| m.contains("extended data stream 5")),
        "{messages:?}"
    );
}

#[skuld::test]
fn exec_ends_with_connection_lost_when_the_vm_process_exits(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);
    let end = exit_on_demand(&fx, &instance);

    let result = std::thread::scope(|scope| {
        let run = scope.spawn(|| instance.run(&Command::new(["hold"])));
        // The command is in flight on the server, and now the VM goes away.
        fx.server.wait_for_commands(1);
        end.send(()).unwrap();
        run.join().unwrap()
    });

    let error = result.expect_err("the VM exited under the command");
    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("the VM process exited"), "{error}");
}

// Copy ================================================================================================================

#[skuld::test]
fn copy_in_and_copy_out_move_a_file_through_the_session(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);
    let up = home.join("up.txt");
    std::fs::write(&up, "going up").unwrap();

    instance.copy_in(&up, "/up.txt").unwrap();
    assert_eq!(std::fs::read(fx.server.root().join("up.txt")).unwrap(), b"going up");

    let down = home.join("down.txt");
    instance.copy_out("/up.txt", &down).unwrap();
    assert_eq!(std::fs::read(&down).unwrap(), b"going up");
}

// One exit watcher ====================================================================================================

#[skuld::test]
fn n_sequential_runs_on_two_handles_make_one_exit_watcher(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let first = running_instance(&fx, home);
    let second = Instance::open_in(&fx.engine, "foo").unwrap();
    let file = home.join("file.txt");
    std::fs::write(&file, "x").unwrap();

    for handle in [&first, &second, &first, &second] {
        handle.run(&Command::new(["echo", "again"])).unwrap();
        handle.copy_in(&file, "/file.txt").unwrap();
    }

    assert_eq!(watched(&fx.engine), [recorded(&first).unwrap().process]);
}

// A machine that does not run =========================================================================================

#[skuld::test]
fn an_instance_that_never_started_is_unavailable_naming_it(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let errors = [
        instance.run(&Command::new(["echo"])).expect_err("nothing runs"),
        instance.reboot().expect_err("nothing runs"),
        instance.copy_out("/x", home).expect_err("nothing runs"),
    ];

    for error in errors {
        assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
        assert!(error.message().contains("foo"), "{error}");
        assert!(error.message().contains("start it"), "{error}");
    }
}

#[skuld::test]
fn a_recorded_machine_the_backend_cannot_find_is_unavailable(#[fixture(unicode_home)] home: &Path) {
    let (fx, _, _) = stub_fixture(home, Attach::Gone);
    let instance = running_instance(&fx, home);

    let error = instance.run(&Command::new(["echo"])).expect_err("the record is stale");

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("foo"), "{error}");
}

// The ssh_config of a running instance ================================================================================

/// `running_instance`, with the `config` and `foo.conf` that a start writes. Returns the two paths.
fn running_instance_with_ssh_files(
    fx: &EngineFixture,
    home: &Path,
) -> (Instance, std::path::PathBuf, std::path::PathBuf) {
    let instance = running_instance(fx, home);
    fx.engine.ssh_configs().ensure_top_level().unwrap();
    let conf = write_conf(&fx.engine, home, "foo");
    (instance, fx.engine.ssh_configs().config(), conf)
}

#[skuld::test]
fn ssh_config_of_a_running_instance_is_the_config_file(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let (instance, config, _) = running_instance_with_ssh_files(&fx, home);

    assert_eq!(instance.ssh_config().unwrap(), config);
}

#[skuld::test]
fn ssh_config_of_an_instance_that_never_started_is_unavailable_naming_it(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.ssh_config().expect_err("nothing runs");

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("foo"), "{error}");
}

#[skuld::test]
fn ssh_config_without_the_instances_conf_is_unavailable_naming_the_file(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let (instance, _, conf) = running_instance_with_ssh_files(&fx, home);
    std::fs::remove_file(&conf).unwrap();

    let error = instance.ssh_config().expect_err("ssh would look the name up in DNS");

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("foo"), "{error}");
    assert!(error.message().contains(&conf.display().to_string()), "{error}");
}

#[skuld::test]
fn ssh_config_without_the_config_file_is_unavailable_naming_the_file(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let (instance, config, _) = running_instance_with_ssh_files(&fx, home);
    std::fs::remove_file(&config).unwrap();

    let error = instance.ssh_config().expect_err("ssh cannot open its configuration");

    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert!(error.message().contains("foo"), "{error}");
    assert!(error.message().contains(&config.display().to_string()), "{error}");
}

// Reboot ==============================================================================================================

#[skuld::test]
fn reboot_resets_in_the_same_process_and_returns_with_a_new_boot_id(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);
    let machine = recorded(&instance);
    let before = fx.server.connections();

    instance.reboot().unwrap();

    assert_eq!(fx.fake.resets(), 1);
    assert_eq!(fx.fake.launches(), 0, "the machine is reset, not launched again");
    assert_eq!(recorded(&instance), machine, "the process and the record are the same");
    assert_eq!(
        fx.server.connections() - before,
        2,
        "the old boot id was read on one connection and the new one on another"
    );
}

#[skuld::test]
fn reboot_waits_until_the_boot_id_differs(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);
    fx.guests.stale_boot_reads(3);
    let before = fx.server.connections();

    instance.reboot().unwrap();

    assert_eq!(
        fx.server.connections() - before,
        1 + 3 + 1,
        "the old id once, three answers that still say the old boot and the new id: each on a new connection"
    );
}

#[skuld::test]
fn reboot_retries_the_boot_id_reads_that_fail_while_the_guest_goes_down(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let instance = running_instance(&fx, home);
    fx.guests.failing_boot_reads(3);
    let before = fx.server.connections();

    instance.reboot().unwrap();

    assert_eq!(
        fx.server.connections() - before,
        1 + 3 + 1,
        "the old id once, three reads that fail and the new id: each on a new connection"
    );
}

#[skuld::test]
fn a_reboot_whose_boot_id_reads_keep_failing_ends_at_the_boot_bound_naming_the_serial_log(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, memory());
    fx.guests.failing_boot_reads(usize::MAX);
    let instance = short_bound_instance(&fx, home);

    let error = instance.reboot().expect_err("the boot id cannot be read");

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("boot bound of 300ms"), "{error}");
    assert!(
        error.message().contains(&home.join("serial.log").display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn a_reboot_whose_boot_id_read_fails_in_another_way_fails_at_once_with_its_kind(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    fx.guests.unreadable_boot_id();
    let instance = running_instance(&fx, home);
    let before = fx.server.connections();

    let error = instance.reboot().expect_err("the boot id read has no status");

    assert_eq!(error.kind(), ErrorKind::NoExitStatus, "{error}");
    assert_eq!(
        fx.server.connections() - before,
        2,
        "the old id once and the failed read"
    );
}

#[skuld::test]
fn a_reboot_of_a_guest_that_prints_no_boot_id_fails_before_the_reset(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    fx.guests.empty_boot_id();
    let instance = running_instance(&fx, home);

    let error = instance.reboot().expect_err("an empty boot id names no boot");

    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("printed no boot id"), "{error}");
    assert_eq!(
        fx.fake.resets(),
        0,
        "the machine is not reset on a boot id that cannot tell boots apart"
    );
}

#[skuld::test]
fn reboot_of_a_machine_that_cannot_be_reset_fails_and_waits_for_nothing(#[fixture(unicode_home)] home: &Path) {
    let (fx, stub, _) = stub_fixture(home, Attach::Running);
    let instance = running_instance(&fx, home);
    let before = fx.server.connections();

    let error = instance.reboot().expect_err("the stub machine has no reset");

    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    assert_eq!(stub.calls(), ["attach"]);
    assert_eq!(fx.server.connections() - before, 1, "only the old boot id was read");
}

/// The running instance `foo`, opened through an engine with a short boot bound.
pub(crate) fn short_bound_instance(fx: &EngineFixture, home: &Path) -> Instance {
    running_instance(fx, home);
    let engine = engine_with_bound(
        fx.env.dirs.clone(),
        &TestHost,
        Some(fx.fake.clone()),
        fx.guests.clone(),
        memory(),
        Duration::from_millis(300),
    );
    Instance::open_in(&engine, "foo").unwrap()
}

/// The instance `foo` and the error of a reboot whose guest never leaves its old boot, under a short boot bound.
fn reboot_that_never_ends(fx: &EngineFixture, home: &Path) -> (Instance, crate::Error) {
    fx.guests.stale_boot_reads(usize::MAX);
    let instance = short_bound_instance(fx, home);
    let error = instance.reboot().expect_err("the guest never leaves its old boot");
    (instance, error)
}

#[skuld::test]
fn a_reboot_whose_guest_never_comes_back_ends_at_the_boot_bound_naming_the_serial_log(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, memory());

    let (_, error) = reboot_that_never_ends(&fx, home);

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    // Whichever step the bound passes in (a connection, the command, the loop between), it is named with the log.
    assert!(error.message().contains("boot bound of 300ms"), "{error}");
    assert!(
        error.message().contains(&home.join("serial.log").display().to_string()),
        "{error}"
    );
}

#[skuld::test]
fn a_failed_reboot_keeps_the_machine_record_and_releases_the_op_lock(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());

    let (instance, _) = reboot_that_never_ends(&fx, home);

    assert_eq!(fx.fake.resets(), 1, "the guest was reset");
    assert!(recorded(&instance).is_some(), "the instance still names its machine");
    assert!(
        instance.dir().try_op_exclusive().unwrap().is_some(),
        "the op lock is released"
    );
}
