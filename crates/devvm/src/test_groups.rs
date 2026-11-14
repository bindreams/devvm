//! The lib's test groups: those of `shared` (which a test binary can include by `#[path]` too) and the ones that need
//! the lib's host and test support.

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

// The tests run the scripts of `guest/windows/wrap.rs` in `pwsh`, whose first statement ends the script in session 0.
test_group!(
    PWSH_WRAP => pwsh_wrap,
    env = "DEVVM_TEST_PWSH_WRAP",
    precondition = host_runs_pwsh_outside_session_0
);

/// Names the Windows installer ISO the `windows_guest` group's tests build their base from.
pub(crate) const WINDOWS_ISO_VAR: &str = "DEVVM_WINDOWS_ISO";

// Builds a Windows guest from an ISO (about 15 minutes) and boots it. It is on unless `DEVVM_TEST_WINDOWS_GUEST=0`, and
// then fails the tests naming the ISO variable where that is not set; only the `vm-windows` job gives one. The tests also
// need the `vm` group's QEMU, so they ask for that group's consent.
test_group!(
    WINDOWS_GUEST => windows_guest,
    env = "DEVVM_TEST_WINDOWS_GUEST",
    precondition = host_has_a_windows_iso
);

fn host_has_a_windows_iso(opt_out: &str) -> Result<(), String> {
    match std::env::var_os(WINDOWS_ISO_VAR) {
        None => Err(format!(
            "the group needs {WINDOWS_ISO_VAR} to name a Windows installer ISO to build the guest from; {opt_out}"
        )),
        Some(iso) if !std::path::Path::new(&iso).is_file() => Err(format!(
            "{WINDOWS_ISO_VAR} names {}, which is not a file; {opt_out}",
            std::path::Path::new(&iso).display()
        )),
        Some(_) => Ok(()),
    }
}

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

/// Asks `pwsh` for its own session id, which is this process's: the wrapped scripts end with 126 in session 0.
fn host_runs_pwsh_outside_session_0(opt_out: &str) -> Result<(), String> {
    let output = std::process::Command::new("pwsh")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Diagnostics.Process]::GetCurrentProcess().SessionId",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("the group needs `pwsh` (PowerShell 7) on the PATH: {e}; {opt_out}"))?;
    let answer = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    match answer.parse::<u32>() {
        Ok(0) => Err(format!(
            "the group needs a host process outside session 0, where every wrapped script ends with 126 before it runs \
             anything; this process and the `pwsh` it started are in session 0; {opt_out}"
        )),
        Ok(_) => Ok(()),
        Err(_) => Err(format!(
            "the group asked `pwsh` for its session id and got {answer:?} (status {:?}, stderr: {}); {opt_out}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}
