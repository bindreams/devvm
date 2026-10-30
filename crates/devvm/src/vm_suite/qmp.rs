//! The QMP client against a live QEMU 11.1, with no guest: it checks that the bundled `qapi` schema (QEMU 9.x era) still
//! fits the QEMU devvm ships.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;

use qapi::Command;
use qapi::qmp;

use crate::backend::qemu::qmp::{Qmp, VmEnd};
use crate::backend::qemu::sidecar::QemuInstall;
use crate::backend::qemu::uds::UnixStream;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::qemu::{BareQemu, qemu_sidecar};

#[skuld::test]
fn qapi_crate_handles_every_command_and_event_on_qemu_11_1(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let mut qemu = BareQemu::spawn(install, home);
    let endpoint = qemu.sock.endpoint.clone();
    let alive = std::sync::Mutex::new(&mut qemu);
    let mut session = Qmp::connect(&endpoint, &|| alive.lock().unwrap().alive(), &MemoryProgress::default()).unwrap();

    // The machine is frozen (-S), and `info usernet` lists the forward with the port QEMU chose.
    assert_eq!(session.query_status().unwrap().state, "prelaunch");
    assert_eq!(session.target_arch().unwrap(), crate::Arch::Amd64);
    let forwards = session.forwarded_ports().unwrap();
    assert_eq!(forwards.len(), 1, "{forwards:?}");
    assert_eq!(forwards[0].guest, 22);
    assert_ne!(forwards[0].host, 0);

    session.cont().unwrap();
    assert_eq!(session.query_status().unwrap().state, "running");

    // `STOP` is an event the crate must parse; `wait_for_end` reports it by the run state it finds.
    session.stop().unwrap();
    assert_eq!(session.wait_for_end().unwrap(), VmEnd::Paused("paused".to_owned()));
    session.cont().unwrap();

    session.system_powerdown().unwrap();
    // `RESET` is awaited by the call itself.
    session.system_reset().unwrap();

    session.quit().unwrap();
    // `SHUTDOWN` arrives, then QEMU closes the socket.
    assert_eq!(session.wait_for_end().unwrap(), VmEnd::Exited);
}

/// The commands, with the argument names, that devvm sends. The names come from the `qapi` crate's own command types.
fn commands_devvm_sends() -> Vec<(&'static str, &'static [&'static str])> {
    vec![
        (qmp::qmp_capabilities::NAME, &[]),
        (qmp::query_status::NAME, &[]),
        (qmp::query_target::NAME, &[]),
        (qmp::stop::NAME, &[]),
        (qmp::cont::NAME, &[]),
        (qmp::system_powerdown::NAME, &[]),
        (qmp::system_reset::NAME, &[]),
        (qmp::quit::NAME, &[]),
        (qmp::human_monitor_command::NAME, &["command-line"]),
    ]
}

#[skuld::test]
fn every_command_devvm_sends_exists_in_the_live_schema(
    #[fixture(qemu_sidecar)] install: &Arc<QemuInstall>,
    #[fixture(unicode_home)] home: &Path,
) {
    let mut qemu = BareQemu::spawn(install, home);
    // A bare session: `query-qmp-schema` is not a command devvm sends.
    let stream = loop {
        match UnixStream::connect(qemu.sock.endpoint.path()) {
            Ok(stream) => break stream,
            Err(e) => assert!(
                qemu.alive(),
                "QEMU exited before its control socket accepted a connection ({e}); its output is in {}",
                home.join("qemu.log").display()
            ),
        }
        std::thread::yield_now();
    };
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut writer = stream;
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut ask = |command: &str| -> serde_json::Value {
        writeln!(writer, "{{\"execute\":\"{command}\"}}").unwrap();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value.get("event").is_none() {
                return value;
            }
        }
    };
    ask("qmp_capabilities");
    let schema = ask("query-qmp-schema");
    let entries = schema["return"].as_array().expect("a schema list");

    for (command, arguments) in commands_devvm_sends() {
        let info = entries
            .iter()
            .find(|entry| entry["meta-type"] == "command" && entry["name"] == command)
            .unwrap_or_else(|| panic!("QEMU has no QMP command {command}"));
        let Some(arg_type) = info["arg-type"].as_str() else {
            assert!(arguments.is_empty(), "{command} takes no arguments in the live schema");
            continue;
        };
        let members = entries
            .iter()
            .find(|entry| entry["name"] == arg_type)
            .and_then(|entry| entry["members"].as_array())
            .unwrap_or_else(|| panic!("the argument type {arg_type} of {command} is missing"));
        for argument in arguments {
            assert!(
                members.iter().any(|member| member["name"] == *argument),
                "{command} has no argument {argument} in the live schema"
            );
        }
    }
}
