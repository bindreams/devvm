//! Tests that rely on what root is not bound by.

/// Panics when the tests run as root, whom mode bits and ownership do not bind: `why` says what the test relies on.
pub fn refuse_root(why: &str) {
    // SAFETY: `geteuid` reads a process attribute.
    assert_ne!(unsafe { libc::geteuid() }, 0, "{why}: run this test as another user");
}
