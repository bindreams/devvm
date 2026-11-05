use std::path::PathBuf;

/// `/`, which root owns. Panics when the tests run as root, which owns it: a test that is refused `/` would act on it.
pub fn a_directory_of_another_account() -> PathBuf {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "root owns `/`, so it is not another account's directory: run this test as another user"
    );
    PathBuf::from("/")
}
