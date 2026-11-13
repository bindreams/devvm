//! A `copy` step's extraction on a Windows guest: the tar the host uploaded goes into the destination with .NET's own
//! reader, because the guest has neither `sh` nor a `tar` devvm can rely on.

use super::WindowsGuest;
use super::wrap::literal;
use crate::guest::GuestOs;
use crate::recipe::Shell;
use crate::{Command, Result, RunAs};

#[cfg(test)]
#[path = "extract_tests.rs"]
mod extract_tests;

#[cfg(all(test, windows))]
#[path = "extract_pwsh_tests.rs"]
mod extract_pwsh_tests;

/// Where a `copy` step's archive is uploaded, in the form a guest path is typed: the first-logon script makes
/// `C:\ProgramData\devvm` with `Users` modify, so the user's SFTP can write there, and SFTP expands no environment
/// variable that `%TEMP%` would need.
pub(super) const SCRATCH_DIR: &str = "C:/ProgramData/devvm";

/// The command that makes `destination`, extracts `archive` into it, removes `archive` whatever the extraction did, and
/// ends with 0 only if all of it worked. Both paths are guest paths as the user types them (`C:/x`), in the script as
/// literals: a `-Command` script takes no arguments, so no quoting of the paths is left to the caller.
///
/// The extraction overwrites files that exist, as `tar -xf` does. An entry the guest cannot make (a symbolic link:
/// the user's token may not create one) ends the step with .NET's message, which names it.
pub(super) fn extract_command(destination: &str, archive: &str) -> Result<Command> {
    Ok(Command {
        argv: WindowsGuest.shell_argv(Some(Shell::Pwsh), &script(destination, archive))?,
        run_as: RunAs::User,
        cwd: None,
    })
}

/// The PowerShell text of [`extract_command`].
fn script(destination: &str, archive: &str) -> String {
    format!(
        "$destination = {}\n\
         $archive = {}\n\
         $status = 0\n\
         try {{\n\
         \x20   [void][System.IO.Directory]::CreateDirectory($destination)\n\
         \x20   [System.Formats.Tar.TarFile]::ExtractToDirectory($archive, $destination, $true)\n\
         }} catch {{\n\
         \x20   [Console]::Error.WriteLine(\"devvm: cannot extract ${{archive}} into ${{destination}}: $($_.Exception.Message)\")\n\
         \x20   $status = 1\n\
         }} finally {{\n\
         \x20   try {{\n\
         \x20       Remove-Item -LiteralPath $archive -Force -ErrorAction Stop\n\
         \x20   }} catch {{\n\
         \x20       [Console]::Error.WriteLine(\"devvm: cannot remove ${{archive}}: $($_.Exception.Message)\")\n\
         \x20       $status = 1\n\
         \x20   }}\n\
         }}\n\
         exit $status\n",
        literal(destination),
        literal(archive),
    )
}
