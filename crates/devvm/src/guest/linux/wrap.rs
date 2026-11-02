//! Turning a `Command` into the string the guest account's shell runs.
//!
//! The string is `bash -lc '<script>'`: a login shell, so tools an earlier step installed (rustup's `~/.cargo/env`,
//! sourced from `~/.profile`) are on `PATH`. The script is `cd`, then `exec --`. The `cd` is `CDPATH= cd -P --`: the
//! profile's `CDPATH` must not redirect a relative directory or make `cd` print the one it chose, and `-P` resolves
//! `..` after a symlink physically, as the kernel does (a plain `cd` folds it textually). The guest account's startup
//! files are the user's: one that redefines `cd` or `exec`, or sets `cdable_vars`, can change what the command does,
//! and devvm does not check.
//! `exec --` ends the builtin's own option parsing, so `-x` and `a=b` are program names (`env(1)` would read them as an
//! option and an assignment even after `--`).
//!
//! Elevated commands run as `exec sudo -n -- bash -c '<the same script>'`: the `cd` and the program both run inside the
//! root shell, so a directory only root can enter works. The interpreter is `bash`, never `sh`: on Ubuntu `/bin/sh` is
//! dash, which has no `exec --`.

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
    let mut exec = String::from("exec --");
    for arg in &cmd.argv {
        exec.push(' ');
        exec.push_str(&quote_checked("an argument", arg)?);
    }
    steps.push(exec);
    Ok(steps.join(" && "))
}

/// `cwd` as the operand of `cd -P --`. `cd -P -- -` still means `$OLDPWD`, so a value starting with `-` is written as an
/// explicit relative path.
fn cd_operand(cwd: &str) -> String {
    if cwd.starts_with('-') {
        format!("./{cwd}")
    } else {
        cwd.to_owned()
    }
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
