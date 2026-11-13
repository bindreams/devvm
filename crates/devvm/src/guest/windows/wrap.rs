//! Turning a `Command` into the PowerShell script ssh-broker's agent runs for the guest account.
//!
//! sshd hands the string to ssh-broker (`<broker> -c "<script>"`), which starts `pwsh.exe -c <script>` in the user's
//! session. The script, in this order:
//!
//! 1. checks its session and ends with 126 in session 0, where a broker without its agent runs it at High (6.5);
//! 2. sets the working directory, if the command has one, and resolves argv[0] as an **application**, never a cmdlet
//!    or an alias; a name that is not found is 127;
//! 3. starts the file found with `System.Diagnostics.Process`, copies the standard streams as raw bytes and ends with
//!    the child's status (the "C2" carrier: every byte of stdout and stderr, and every 32-bit status, measured exact).
//!    The arguments are built on the host in the three forms the guest can need, and the guest picks by the file it
//!    found: a list for a program, the batch-file command line of [`cmdquote`] for a `.cmd` or `.bat` file, and the
//!    command line of `cmd.exe /c <text>` for `cmd.exe`. No output encoding is set here: it would re-encode every
//!    user program's output.
//!
//! An elevated command is the same script run by the helper of [`elevate`].

use super::cmdquote;
use super::elevate;
use crate::{Command, Error, ErrorKind, Result, RunAs};

#[cfg(test)]
#[path = "wrap_tests.rs"]
mod wrap_tests;

#[cfg(all(test, windows))]
#[path = "wrap_pwsh_tests.rs"]
mod wrap_pwsh_tests;

/// Where ssh-broker is installed, whose `-c "<script>"` is the line sshd builds.
pub(super) const BROKER_EXE: &str = r"C:\ProgramData\ssh-broker\ssh-broker.exe";
/// The PowerShell 7 the broker starts for the script (`config.yaml`'s `shell.start`).
pub(super) const PWSH_EXE: &str = r"C:\Program Files\PowerShell\7\pwsh.exe";

/// The longest command line `CreateProcess` takes, in UTF-16 units.
pub(super) const LINE_LIMIT: usize = 32_767;

/// What a command prints in session 0, with status 126.
pub(super) const NOT_IN_SESSION: &str = "the command did not reach the user's session";

/// The first statement of every script and the helper's caller: in session 0 (the agent died, or its session logged
/// off) the shell is a plain sshd one, at High, and nothing is run.
pub(super) const SESSION_CHECK: &str = "if ([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) { [Console]::Error.WriteLine('devvm: the command did not reach the user''s session'); exit 126 }\n";

/// The copy of the standard streams and the end with the child's status, after the arguments are set.
const CARRY: &str = "$p = [Diagnostics.Process]::Start($psi)
$stdout = [Console]::OpenStandardOutput()
$stderr = [Console]::OpenStandardError()
$ti = [Console]::OpenStandardInput().CopyToAsync($p.StandardInput.BaseStream)
$to = $p.StandardOutput.BaseStream.CopyToAsync($stdout)
$te = $p.StandardError.BaseStream.CopyToAsync($stderr)
$both = [Threading.Tasks.Task]::WhenAll($to, $te)
if ([Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]@($ti, $both)) -eq 0) { try { $p.StandardInput.Close() } catch { } }
$both.Wait()
$p.WaitForExit()
exit $p.ExitCode
";

/// `text` as a PowerShell single-quoted string. PowerShell reads U+2018 to U+201B as a single quote too, so each is
/// doubled like `'`.
pub(super) fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        out.push(c);
        if matches!(c, '\'' | '\u{2018}'..='\u{201b}') {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// The script sshd hands to ssh-broker for `cmd`; `usage` for an empty argv or working directory, a NUL byte, or a
/// script that makes either command line over [`LINE_LIMIT`].
pub(super) fn wrap(cmd: &Command) -> Result<String> {
    let script = script_unchecked(cmd)?;
    let (sshd, broker) = command_line_lengths(&script);
    if sshd.max(broker) > LINE_LIMIT {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "the command is too long for a Windows guest: sshd's command line for it is {sshd} UTF-16 units and \
                 ssh-broker's for PowerShell is {broker}, and Windows takes {LINE_LIMIT} at most (quoting makes the \
                 lines longer than the command)"
            ),
        ));
    }
    Ok(script)
}

/// [`wrap`] before the length check.
fn script_unchecked(cmd: &Command) -> Result<String> {
    let user = user_script(cmd)?;
    Ok(match cmd.run_as {
        RunAs::User => user,
        RunAs::Elevated => elevate::caller_script(&user),
    })
}

/// The lengths in UTF-16 units of the two command lines `script` goes through: `<broker> -c <script>` that sshd builds
/// and `"<pwsh>" -c <script>` that the broker builds, each with the script as one argument in `CommandLineToArgvW`
/// quoting, which is how both quote it (`escaping: msvc`; sshd's own escaping is measured by the guest tests).
fn command_line_lengths(script: &str) -> (usize, usize) {
    let units: Vec<u16> = script.encode_utf16().collect();
    let quoted = cosca::quote::windows::join_wide(&[&units]).len();
    let prefix = |program: &str, quote: bool| program.encode_utf16().count() + if quote { 2 } else { 0 } + " -c ".len();
    (prefix(BROKER_EXE, false) + quoted, prefix(PWSH_EXE, true) + quoted)
}

fn user_script(cmd: &Command) -> Result<String> {
    if cmd.argv.is_empty() {
        return Err(Error::new(ErrorKind::Usage, "a command needs a non-empty argv"));
    }
    let nul = |what: &str| {
        Error::new(
            ErrorKind::Usage,
            format!("{what} contains a NUL byte, which a command line cannot carry"),
        )
    };
    if let Some(cwd) = &cmd.cwd {
        if cwd.is_empty() {
            return Err(Error::new(ErrorKind::Usage, "the working directory is empty"));
        }
        if cwd.contains('\0') {
            return Err(nul("the working directory"));
        }
    }
    if cmd.argv.iter().any(|arg| arg.contains('\0')) {
        return Err(nul("an argument"));
    }

    let program = &cmd.argv[0];
    let args = &cmd.argv[1..];
    let mut script = String::from("$ErrorActionPreference = 'Stop'\n");
    script.push_str(SESSION_CHECK);
    if let Some(cwd) = &cmd.cwd {
        script.push_str(&format!("Set-Location -LiteralPath {}\n", literal(cwd)));
    }
    script.push_str(&format!(
        "$app = Get-Command -CommandType Application -Name ([Management.Automation.WildcardPattern]::Escape({})) -ErrorAction SilentlyContinue | Select-Object -First 1\n",
        literal(program)
    ));
    script.push_str(&format!(
        "if (-not $app) {{ [Console]::Error.WriteLine({}); exit 127 }}\n",
        literal(&format!("devvm: {program}: command not found"))
    ));
    script.push_str("$psi = [Diagnostics.ProcessStartInfo]::new($app.Source)\n");
    if cmd.cwd.is_some() {
        script.push_str("$psi.WorkingDirectory = (Get-Location).ProviderPath\n");
    }
    script.push_str(
        "$psi.UseShellExecute = $false\n$psi.RedirectStandardInput = $true\n$psi.RedirectStandardOutput = $true\n$psi.RedirectStandardError = $true\n",
    );
    script.push_str("$ext = [IO.Path]::GetExtension($app.Source)\n");
    script.push_str("if ($ext -ieq '.cmd' -or $ext -ieq '.bat') {\n");
    script.push_str(&batch_form(args));
    script.push('}');
    if let Some(form) = cmdquote::command_line_arguments(&cmd.argv) {
        script.push_str(" elseif ([IO.Path]::GetFileName($app.Source) -ieq 'cmd.exe') {\n");
        script.push_str(&match form {
            Ok(line) => format!("$psi.Arguments = {}\n", literal(&line)),
            Err(why) => refusal(&why),
        });
        script.push('}');
    }
    script.push_str(" else {\n");
    for arg in args {
        script.push_str(&format!("$psi.ArgumentList.Add({})\n", literal(arg)));
    }
    script.push_str("}\n");
    script.push_str(CARRY);
    Ok(script)
}

/// The arguments of a batch file: std's own command line, `cmd.exe /e:ON /v:OFF /d /c ""<file>" <arguments>"`, or the
/// refusal of an argument std refuses.
fn batch_form(args: &[String]) -> String {
    match cmdquote::batch_arguments(args) {
        Ok(tail) => format!(
            "$psi.FileName = [IO.Path]::Combine([Environment]::SystemDirectory, 'cmd.exe')\n$psi.Arguments = '/e:ON /v:OFF /d /c \"\"' + $app.Source + '\"' + {} + '\"'\n",
            literal(&tail)
        ),
        Err(why) => refusal(&why),
    }
}

/// What the script does for an argument `cmd.exe` cannot be given: says so and does not run the command.
fn refusal(why: &str) -> String {
    format!(
        "[Console]::Error.WriteLine({})\nexit 126\n",
        literal(&format!("devvm: {why}, and the program is run by cmd.exe"))
    )
}
