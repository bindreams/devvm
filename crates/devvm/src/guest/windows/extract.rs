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
/// One `TarReader` loop writes each entry on its own, as `tar -xf` does. A file that exists is overwritten: its
/// ReadOnly, Hidden and System attributes are cleared first (the write fails on them) and are not restored, because
/// the archive says what the file is. A regular file whose entry has no owner-write bit gets the ReadOnly attribute
/// afterwards; directories stay writable. If writing an entry fails, that file gets its attributes back and the step
/// ends naming the entry; the files after it are untouched. An entry that is neither a file nor a directory (a
/// symbolic link: the user's token may not make one) or that lies outside `destination` ends the step naming it,
/// before anything is written for it.
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
         $cleared = [int]([System.IO.FileAttributes]::ReadOnly -bor [System.IO.FileAttributes]::Hidden -bor [System.IO.FileAttributes]::System)\n\
         try {{\n\
         \x20   [void][System.IO.Directory]::CreateDirectory($destination)\n\
         \x20   $root = [System.IO.Path]::GetFullPath($destination).TrimEnd('\\', '/') + [System.IO.Path]::DirectorySeparatorChar\n\
         \x20   $stream = [System.IO.File]::OpenRead($archive)\n\
         \x20   try {{\n\
         \x20       $reader = [System.Formats.Tar.TarReader]::new($stream)\n\
         \x20       while ($null -ne ($entry = $reader.GetNextEntry($false))) {{\n\
         \x20           $path = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($destination, $entry.Name))\n\
         \x20           if (-not ($path + [System.IO.Path]::DirectorySeparatorChar).StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) {{\n\
         \x20               throw \"the entry $($entry.Name) lies outside the destination\"\n\
         \x20           }}\n\
         \x20           if ($entry.EntryType -eq [System.Formats.Tar.TarEntryType]::Directory) {{\n\
         \x20               [void][System.IO.Directory]::CreateDirectory($path)\n\
         \x20               continue\n\
         \x20           }}\n\
         \x20           if ($entry.EntryType -notin [System.Formats.Tar.TarEntryType]::RegularFile, [System.Formats.Tar.TarEntryType]::V7RegularFile) {{\n\
         \x20               throw \"the entry $($entry.Name) is a $($entry.EntryType), which a Windows guest does not extract\"\n\
         \x20           }}\n\
         \x20           [void][System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($path))\n\
         \x20           $saved = $null\n\
         \x20           try {{\n\
         \x20               if ([System.IO.File]::Exists($path)) {{\n\
         \x20                   $saved = [System.IO.File]::GetAttributes($path)\n\
         \x20                   $rest = [int]$saved -band -bnot $cleared\n\
         \x20                   if ($rest -eq 0) {{ $rest = [int][System.IO.FileAttributes]::Normal }}\n\
         \x20                   [System.IO.File]::SetAttributes($path, [System.IO.FileAttributes]$rest)\n\
         \x20               }}\n\
         \x20               $entry.ExtractToFile($path, $true)\n\
         \x20               if (-not $entry.Mode.HasFlag([System.IO.UnixFileMode]::UserWrite)) {{\n\
         \x20                   [System.IO.File]::SetAttributes($path, [System.IO.File]::GetAttributes($path) -bor [System.IO.FileAttributes]::ReadOnly)\n\
         \x20               }}\n\
         \x20           }} catch {{\n\
         \x20               $message = $_.Exception.Message\n\
         \x20               if ($null -ne $saved) {{\n\
         \x20                   try {{\n\
         \x20                       [System.IO.File]::SetAttributes($path, $saved)\n\
         \x20                   }} catch {{\n\
         \x20                       $message += \" (its attributes could not be restored: $($_.Exception.Message))\"\n\
         \x20                   }}\n\
         \x20               }}\n\
         \x20               throw \"cannot write $($entry.Name): $message\"\n\
         \x20           }}\n\
         \x20       }}\n\
         \x20   }} finally {{\n\
         \x20       $stream.Dispose()\n\
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
