//! Creating symlinks from tests that run on every OS. On Windows this needs Developer Mode or an elevated token, and
//! a test that cannot get one fails naming that.

use std::path::Path;

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
            "cannot create the symlink {} -> {}: {e} (on Windows this needs Developer Mode or an elevated token)",
            link.display(),
            target.display()
        )
    })
}
