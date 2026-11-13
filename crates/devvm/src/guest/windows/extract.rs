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
/// The extraction overwrites files that exist, as `tar -xf` does, and a file that is read-only is cleared of the
/// attribute first. A regular file whose entry has no owner-write bit gets the ReadOnly attribute afterwards: the
/// archive says what `tar -xf` would make read-only. Directories stay writable. An entry the guest cannot make (a
/// symbolic link: the user's token may not create one) ends the step with .NET's message, which names it.
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
         \x20   $files = [System.Collections.Generic.List[object]]::new()\n\
         \x20   $stream = [System.IO.File]::OpenRead($archive)\n\
         \x20   try {{\n\
         \x20       $reader = [System.Formats.Tar.TarReader]::new($stream)\n\
         \x20       while ($null -ne ($entry = $reader.GetNextEntry($false))) {{\n\
         \x20           if ($entry.EntryType -in [System.Formats.Tar.TarEntryType]::RegularFile, [System.Formats.Tar.TarEntryType]::V7RegularFile) {{\n\
         \x20               $files.Add([pscustomobject]@{{\n\
         \x20                   Path = [System.IO.Path]::Combine($destination, $entry.Name)\n\
         \x20                   ReadOnly = -not $entry.Mode.HasFlag([System.IO.UnixFileMode]::UserWrite)\n\
         \x20               }})\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }} finally {{\n\
         \x20       $stream.Dispose()\n\
         \x20   }}\n\
         \x20   foreach ($file in $files) {{\n\
         \x20       if ([System.IO.File]::Exists($file.Path)) {{\n\
         \x20           $attributes = [System.IO.File]::GetAttributes($file.Path)\n\
         \x20           if ($attributes.HasFlag([System.IO.FileAttributes]::ReadOnly)) {{\n\
         \x20               [System.IO.File]::SetAttributes($file.Path, $attributes -bxor [System.IO.FileAttributes]::ReadOnly)\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }}\n\
         \x20   [System.Formats.Tar.TarFile]::ExtractToDirectory($archive, $destination, $true)\n\
         \x20   foreach ($file in $files) {{\n\
         \x20       if ($file.ReadOnly) {{\n\
         \x20           [System.IO.File]::SetAttributes($file.Path, [System.IO.File]::GetAttributes($file.Path) -bor [System.IO.FileAttributes]::ReadOnly)\n\
         \x20       }}\n\
         \x20   }}\n\
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
