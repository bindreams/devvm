//! The lib's test groups: those of `shared` (which a test binary can include by `#[path]` too) and the ones that need
//! the lib's host and test support. A test joins any of them through `crate::test_groups`.

#[macro_use]
mod shared;
pub(crate) use shared::*;

use crate::host;

/// Set, to its manifest's locale, in the child `host_tests` pins to code page 1252: stops it from spawning another.
pub(crate) const PINNED_CHILD: &str = "DEVVM_PINNED_CHILD";

test_group!(ACP_1252 => acp_1252, env = "DEVVM_TEST_ACP_1252", precondition = host_has_code_page_1252);
test_group!(
    UNICODE_SOCKET_DIR => unicode_socket_dir,
    env = "DEVVM_TEST_UNICODE_SOCKET_DIR",
    precondition = socket_dir_fits_unicode
);

fn host_has_code_page_1252(opt_out: &str) -> Result<(), String> {
    let code_page = host::current().ansi_code_page();
    if code_page == Some(1252) {
        return Ok(());
    }
    if std::env::var_os(PINNED_CHILD).is_some() {
        return Err(format!(
            "the manifest's activeCodePage did not pin the process to ANSI code page 1252; its code page is \
             {code_page:?}"
        ));
    }
    match code_page {
        Some(other) => Err(format!(
            "the group needs ANSI code page 1252, and this host's is {other}; {opt_out}"
        )),
        None => Err(format!(
            "the group needs ANSI code page 1252, and this host has no ANSI code page; {opt_out}"
        )),
    }
}

fn socket_dir_fits_unicode(opt_out: &str) -> Result<(), String> {
    crate::test_support::qemu::unicode_socket_dir_fits().map_err(|why| {
        format!("the group needs a socket directory with a space and a non-ASCII character: {why}; {opt_out}")
    })
}
