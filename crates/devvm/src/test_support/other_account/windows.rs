use std::path::PathBuf;

use crate::host::windows::windows_tests::is_owned_by_this_account;

/// `%ProgramData%`, which the system owns. Panics when the tests run as the account that owns it: a test that is
/// refused it would act on it.
pub fn a_directory_of_another_account() -> PathBuf {
    let dir = PathBuf::from(std::env::var_os("ProgramData").expect("ProgramData is set"));
    assert!(
        !is_owned_by_this_account(&dir),
        "{} is owned by the account the tests run as, so it is not another account's directory: run this test as another user",
        dir.display()
    );
    dir
}
