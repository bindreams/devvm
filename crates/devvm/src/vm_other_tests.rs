use std::path::Path;
use std::sync::Arc;

use crate::engine::engine_tests::{engine_fixture, instance_in};
use crate::test_support::fixtures::{MemoryProgress, anna_home};
use crate::{ErrorKind, Instance};

#[skuld::test]
fn start_on_a_host_that_cannot_freeze_a_process_is_not_supported_and_records_nothing(
    #[fixture(anna_home)] home: &Path,
) {
    let fx = engine_fixture(home, Arc::new(MemoryProgress::default()));
    let dir = instance_in(&fx, "foo", home);
    let instance = Instance::open_in(&fx.engine, "foo").unwrap();

    let error = instance.start().expect_err("this host cannot spawn a process frozen");

    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert!(error.message().contains("Windows"), "{}", error.message());
    assert_eq!(fx.fake.launches(), 0);
    assert!(dir.record().unwrap().machine.is_none());
    dir.op_exclusive().expect("the failed start released the op lock");
}
