//! Creating symlinks from tests that run on every OS. On Windows this needs Developer Mode or an elevated token: a test
//! that makes one joins the `symlinks` group (`#[fixture(symlinks)] _group: &Group`), and fails naming the opt-out
//! where the host lacks the right. The rule covers any path that makes a host symlink, not only calls of [`symlink`]:
//! a test that reaches `Host::make_symlink` joins too.

use std::path::Path;

use crate::test_groups::symlinks;

#[cfg(unix)]
#[path = "symlink/unix.rs"]
mod imp;
#[cfg(not(unix))]
#[path = "symlink/other.rs"]
mod imp;

/// Makes `link` a symlink to `target`. A failure names the opt-out only when the error is the one a host without the
/// right gives.
pub fn symlink(target: &Path, link: &Path) {
    imp::symlink(target, link).unwrap_or_else(|error| {
        let message = format!(
            "cannot create the symlink {} -> {}: {error}",
            link.display(),
            target.display()
        );
        if imp::lacks_the_right(&error) {
            panic!(
                "{message} (this needs Developer Mode or an elevated token), or {}",
                symlinks::OPT_OUT
            )
        }
        panic!("{message}")
    })
}

#[cfg(test)]
#[path = "symlink_tests.rs"]
mod symlink_tests;
