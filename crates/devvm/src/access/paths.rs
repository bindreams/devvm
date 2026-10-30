//! Guest paths as users type them: `~` is the guest user's home.

use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "paths_tests.rs"]
mod paths_tests;

/// Whether `path` starts with `~user`: another user's home, which devvm does not resolve.
fn is_tilde_user(path: &str) -> bool {
    path.strip_prefix('~')
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
}

/// Rejects `~user...`; every other path, `~` and `~/x` included, passes.
pub(crate) fn check_guest_path(path: &str) -> Result<()> {
    if is_tilde_user(path) {
        return Err(Error::new(
            ErrorKind::Usage,
            format!("{path:?} names another user's home; only ~ and ~/... are supported"),
        ));
    }
    Ok(())
}

/// `~` and `~/x` made relative to `home`; every other path, a `~user` one included, is returned as is. Total: call
/// [`check_guest_path`] first to refuse `~user`.
pub(crate) fn expand_guest_path(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    match path.strip_prefix('~') {
        Some("") => home.to_owned(),
        Some(rest) if rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_owned(),
    }
}
