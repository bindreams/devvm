use std::path::Path;
use std::sync::Arc;

use cosca::identity::{ProcessId, ProcessIdRecord};
use secrecy::ExposeSecret;

use crate::access::SshTarget;
use crate::access::keys::InstanceKeys;
use crate::engine::engine_tests::{EngineFixture, engine_fixture, memory};
use crate::engine::ready::connect_ready;
use crate::guest::GuestOs;
use crate::instance::name::InstanceName;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::{ErrorKind, GuestKind};

/// The record of a VM process that runs for as long as the test does: this process.
fn running_vm() -> ProcessIdRecord {
    ProcessId::current()
        .to_record()
        .expect("this process has an identity record")
}

/// A target at the fixture's server, which now trusts a fresh pair of keys.
fn target_at(fx: &EngineFixture, home: &Path) -> SshTarget {
    let keys = InstanceKeys::generate().unwrap();
    fx.server
        .install(&keys.client_public, keys.host_private.expose_secret());
    SshTarget {
        addr: fx.server.addr(),
        user: "user".to_owned(),
        client_private: keys.client_private,
        host_public: keys.host_public,
        serial_log: home.join("serial.log"),
    }
}

fn linux(fx: &EngineFixture) -> Arc<dyn GuestOs> {
    use crate::guest::Guests;
    fx.guests.os(GuestKind::Linux).unwrap()
}

fn name() -> InstanceName {
    InstanceName::parse("foo").unwrap()
}

#[skuld::test]
fn a_ready_guest_gives_a_session_that_runs_commands(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let target = target_at(&fx, home);

    let session = connect_ready(&fx.engine, &*linux(&fx), &name(), &target, &running_vm()).unwrap();

    assert_eq!(session.exec_collect("echo up").unwrap().stdout, b"up\n");
}

#[skuld::test]
fn a_degraded_first_boot_is_reported_by_name_and_is_not_an_error(#[fixture(unicode_home)] home: &Path) {
    let progress = Arc::new(MemoryProgress::default());
    let fx = engine_fixture(home, progress.clone());
    let target = target_at(&fx, home);
    fx.guests.degraded("degraded done: one module failed");

    connect_ready(&fx.engine, &*linux(&fx), &name(), &target, &running_vm()).unwrap();

    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m == "instance foo: the boot finished with problems: degraded done: one module failed"),
        "{messages:?}"
    );
}

#[skuld::test]
fn a_vm_process_that_is_already_gone_is_connection_lost_naming_the_endpoint(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let target = target_at(&fx, home);
    let mut gone = running_vm();
    // A process of this id that started at another moment is not this one.
    gone.token = gone.token.wrapping_add(1);

    let error = connect_ready(&fx.engine, &*linux(&fx), &name(), &target, &gone)
        .err()
        .expect("there is no VM to wait for");

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(
        error
            .message()
            .contains(&format!("waiting for SSH on {} to answer", target.addr)),
        "{error}"
    );
}
