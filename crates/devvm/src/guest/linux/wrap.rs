//! Turning a `Command` into the string the guest account's shell runs.
//!
//! The string is `bash -lc '<script>'`: a login shell, so tools an earlier step installed (rustup's `~/.cargo/env`,
//! sourced from `~/.profile`) are on `PATH`. The script is `cd`, `export`, then `exec --`. The `cd` is
//! `CDPATH= cd --`: the profile's `CDPATH` must not redirect a relative directory or make `cd` print the one it
//! chose. `env(1)` is not used: it reads a leading `-` in the program name as an option and a name containing `=` as
//! an assignment, and `--` does not prevent either. `exec --` ends the builtin's own option parsing, so `-x` and
//! `a=b` are program names.
//!
//! Elevated commands run as `exec sudo -n -- bash -c '<the same script>'`: the `cd`, the `export`s and the program all
//! run inside the root shell, so sudo's `env_reset` cannot drop the variables and a directory only root can enter
//! works. The interpreter is `bash`, never `sh`: on Ubuntu `/bin/sh` is dash, which has no `exec --`.

use crate::{Command, Error, ErrorKind, Result, RunAs};

#[cfg(test)]
#[path = "wrap_tests.rs"]
mod wrap_tests;

/// The string sshd hands to the account's shell for `cmd`.
pub(super) fn wrap(cmd: &Command) -> Result<String> {
    Ok(format!("bash -lc {}", quote(&script(cmd)?)))
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
        steps.push(format!(
            "CDPATH= cd -- {}",
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
            if READONLY_VARIABLES.contains(&name.as_str()) {
                return Err(Error::new(
                    ErrorKind::Usage,
                    format!("`{name}` is a readonly variable in bash and cannot be set for a command"),
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

/// The variables a fresh bash 5.2 marks readonly (`readonly -p`): `export` of one fails, and the command never runs.
const READONLY_VARIABLES: [&str; 6] = ["BASHOPTS", "BASH_VERSINFO", "EUID", "PPID", "SHELLOPTS", "UID"];

/// `cwd` as the operand of `cd --`. `cd -- -` still means `$OLDPWD`, so a value starting with `-` is written as an
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
