use std::path::PathBuf;

use crate::test_support::root::refuse_root;

/// `/`, which root owns. Panics when the tests run as root, which owns it: a test that is refused `/` would act on it.
pub fn a_directory_of_another_account() -> PathBuf {
    refuse_root("root owns `/`, so it is not another account's directory");
    PathBuf::from("/")
}
