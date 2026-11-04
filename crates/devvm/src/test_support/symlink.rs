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

/// Makes `link` a symlink to `target`.
pub fn symlink(target: &Path, link: &Path) {
    imp::symlink(target, link).unwrap_or_else(|e| {
        panic!(
            "cannot create the symlink {} -> {}: {e} (on Windows this needs Developer Mode or an elevated token), or {}",
            link.display(),
            target.display(),
            symlinks::OPT_OUT
        )
    })
}

#[cfg(test)]
#[path = "symlink_tests.rs"]
mod symlink_tests;
