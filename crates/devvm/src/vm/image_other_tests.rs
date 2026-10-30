use std::path::Path;
use std::sync::Arc;

use crate::engine::engine_tests::{engine_fixture, image_of};
use crate::instance::registry::registry_tests::lock_files_in;
use crate::store::layers::layers_tests::names;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::{ErrorKind, StartOptions};

#[skuld::test]
fn image_start_on_a_host_that_cannot_freeze_a_process_is_not_supported_and_leaves_nothing(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = engine_fixture(home, Arc::new(MemoryProgress::default()));
    let image = image_of(&fx.engine);

    let error = image
        .start(&StartOptions::default())
        .err()
        .expect("no frozen spawn here");

    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert_eq!(fx.fake.launches(), 0);
    assert!(
        names(&fx.env.dirs.instances()).is_empty(),
        "the new instance was removed"
    );
    assert!(lock_files_in(&fx.env.dirs).is_empty());
}
