//! Turning a `Command` into the string the guest account's shell runs.
//!
//! The string is `bash -lc '<script>'`: a login shell, so tools an earlier step installed (rustup's `~/.cargo/env`,
//! sourced from `~/.profile`) are on `PATH`. The script is `cd`, `export`, then `exec --`. The `cd` is
//! `CDPATH= cd -P --`: the profile's `CDPATH` must not redirect a relative directory or make `cd` print the one it
//! chose, and `-P` resolves `..` after a symlink physically, as the kernel does (a plain `cd` folds it textually).
//! The guest account's startup files are the user's: one that redefines `cd`, `export` or `exec`, or sets
//! `cdable_vars`, changes what the command does, and devvm does not check.
//! `env(1)` is not used: it reads `-x` as an option and `a=b` as an assignment even after `--`.
//! `exec --` ends the builtin's own option parsing, so `-x` and `a=b` are program names.
//!
//! Elevated commands run as `exec sudo -n -- bash -c '<the same script>'`: the `cd`, the `export`s and the program all
//! run inside the root shell, so sudo's `env_reset` cannot drop the variables and a directory only root can enter
//! works. The interpreter is `bash`, never `sh`: on Ubuntu `/bin/sh` is dash, which has no `exec --`.

use crate::{Command, Error, ErrorKind, Result, RunAs};

#[cfg(test)]
#[path = "wrap_tests.rs"]
mod wrap_tests;

/// The longest single argument Linux passes to a program (`MAX_ARG_STRLEN` is 131072, with the NUL; measured in
/// ubuntu:24.04 and bash:5.2: 131071 bytes run, 131072 fail with E2BIG).
const LONGEST_ARGUMENT: usize = 131_071;

/// The string sshd hands to the account's shell for `cmd`.
pub(super) fn wrap(cmd: &Command) -> Result<String> {
    let line = format!("bash -lc {}", quote(&script(cmd)?));
    // sshd passes `line` to the login shell as one argument, and the shell passes the script to `bash -lc` as one more
    // (and `sudo`'s `bash -c` the inner script). `line` contains the script, which contains the inner one, so
    // `line` is the longest of them.
    if line.len() > LONGEST_ARGUMENT {
        return Err(Error::new(
            ErrorKind::Usage,
            format!(
                "the command is {} bytes as a shell command line, but the guest's kernel refuses any single argument \
                 over {LONGEST_ARGUMENT} bytes (quoting makes the line longer than the command)",
                line.len()
            ),
        ));
    }
    Ok(line)
}

/// What `bash -lc` runs for `cmd`.
pub(super) fn script(cmd: &Command) -> Result<String> {
    let user = user_script(cmd)?;
    Ok(match cmd.run_as {
        RunAs::User => user,
        RunAs::Elevated => format!("exec sudo -n -- bash -c {}", quote(&user)),
    })
}

fn user_script(cmd: &Command) -> Result<String> {
    if cmd.argv.is_empty() {
        return Err(Error::new(ErrorKind::Usage, "a command needs a non-empty argv"));
    }
    let mut steps = Vec::new();
    if let Some(cwd) = &cmd.cwd {
        if cwd.is_empty() {
            return Err(Error::new(ErrorKind::Usage, "the working directory is empty"));
        }
        steps.push(format!(
            "CDPATH= cd -P -- {}",
            quote_checked("the working directory", &cd_operand(cwd))?
        ));
    }
    if !cmd.env.is_empty() {
        let mut export = String::from("export");
        for (name, value) in &cmd.env {
            if !is_variable_name(name) {
                return Err(Error::new(
                    ErrorKind::Usage,
                    format!("`{name}` is not a valid environment variable name (expected [A-Za-z_][A-Za-z0-9_]*)"),
                ));
            }
            if BASH_OWNED_VARIABLES.contains(&name.as_str()) {
                return Err(Error::new(
                    ErrorKind::Usage,
                    format!("`{name}` is a variable bash owns and cannot be set for a command"),
                ));
            }
            export.push_str(&format!(" {name}={}", quote_checked("an environment value", value)?));
        }
        steps.push(export);
    }
    let mut exec = String::from("exec --");
    for arg in &cmd.argv {
        exec.push(' ');
        exec.push_str(&quote_checked("an argument", arg)?);
    }
    steps.push(exec);
    Ok(steps.join(" && "))
}

/// The names `export NAME=v` does not pass to the program unchanged (bash 5.2.21 and 5.2.37, `export NAME=probe && exec
/// -- printenv NAME` after a command has run, as in a login shell):
/// - readonly, so the `export` fails and the command never runs: `BASHOPTS`, `BASH_VERSINFO`, `EUID`, `PPID`,
///   `SHELLOPTS`, `UID`;
/// - the program gets bash's value, or the assignment changes it: `RANDOM`, `SRANDOM`, `BASHPID`, `LINENO`, `HISTCMD`,
///   `EPOCHSECONDS`, `EPOCHREALTIME`, `BASH_COMMAND`, `BASH_SUBSHELL`, `SHLVL`, `SECONDS`, `OPTIND`, `_`;
/// - the program gets nothing: `GROUPS`, `FUNCNAME`, `DIRSTACK`, `BASH_ALIASES`, `BASH_CMDS`, `BASH_LINENO`,
///   `BASH_SOURCE`, `BASH_ARGC`, `BASH_ARGV`, `PIPESTATUS` (it arrives in a shell that has run no command, but the
///   script runs in a login shell, whose profile has);
/// - the program does not start: `EXECIGNORE`. Its patterns drop executables from the `PATH` search of `exec`, so
///   `export EXECIGNORE='*printenv' && exec -- printenv EXECIGNORE` exits 127, "printenv: not found" (`env(1)` would
///   run the program).
const BASH_OWNED_VARIABLES: [&str; 30] = [
    "BASHOPTS",
    "BASH_VERSINFO",
    "EUID",
    "PPID",
    "SHELLOPTS",
    "UID",
    "RANDOM",
    "SRANDOM",
    "BASHPID",
    "LINENO",
    "HISTCMD",
    "EPOCHSECONDS",
    "EPOCHREALTIME",
    "BASH_COMMAND",
    "BASH_SUBSHELL",
    "SHLVL",
    "SECONDS",
    "OPTIND",
    "_",
    "GROUPS",
    "FUNCNAME",
    "DIRSTACK",
    "BASH_ALIASES",
    "BASH_CMDS",
    "BASH_LINENO",
    "BASH_SOURCE",
    "BASH_ARGC",
    "BASH_ARGV",
    "PIPESTATUS",
    "EXECIGNORE",
];

/// `cwd` as the operand of `cd -P --`. `cd -P -- -` still means `$OLDPWD`, so a value starting with `-` is written as an
/// explicit relative path.
fn cd_operand(cwd: &str) -> String {
    if cwd.starts_with('-') {
        format!("./{cwd}")
    } else {
        cwd.to_owned()
    }
}

fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `text` as one single-quoted word: `'` becomes `'\''`.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// `quote`, after refusing what no shell string can carry: a NUL byte ends a C string.
fn quote_checked(what: &str, text: &str) -> Result<String> {
    if text.contains('\0') {
        return Err(Error::new(
            ErrorKind::Usage,
            format!("{what} contains a NUL byte, which a shell command line cannot carry"),
        ));
    }
    Ok(quote(text))
}
