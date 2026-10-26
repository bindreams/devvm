use crate::contain::{Lifetime, VmProcess};
use crate::test_support::fake_vm;
use crate::{ErrorKind, host};

#[skuld::test]
fn spawning_frozen_is_refused_as_not_supported_before_anything_runs() {
    let dir = skuld::TempDir::new().unwrap();
    let port = fake_vm::free_port();
    let error = VmProcess::spawn_frozen(host::current(), fake_vm::command(port, &dir), Lifetime::DiesWithOwner)
        .err()
        .expect("this host cannot spawn a process frozen");
    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert!(error.message().contains("Windows"), "{}", error.message());
    assert!(fake_vm::refuses(port));
}
