//! How `cmd.exe` reads the arguments of a batch file: a port of Rust std's rules for running a `.bat` or `.cmd` (the fix
//! of CVE-2024-24576), `append_bat_arg` and `make_bat_command_line` in `library/std/src/sys/args/windows.rs`. It is the
//! one copy in this repository: cosca's `quote::windows` quotes for `CommandLineToArgvW` and its `batch_gate` refuses
//! batch targets, and ssh-broker's shim packs one command for a shell, so none of them quotes for `cmd.exe`.
//!
//! The arguments std refuses are refused: a NUL, a `\r` or a `\n` (cmd ends the command line there).

#[cfg(test)]
#[path = "cmdquote_tests.rs"]
mod cmdquote_tests;

/// Why `cmd.exe` cannot be given `arg`, if it cannot.
fn refusal(arg: &str) -> Option<&'static str> {
    if arg.contains('\0') {
        Some("contains a NUL")
    } else if arg.contains(['\r', '\n']) {
        Some("contains a line break, which ends a cmd.exe command line")
    } else {
        None
    }
}

/// `args` as the words of a batch file's command line, each after a space: quoted unless every character is known to be
/// harmless, a `"` doubled and a `%` followed by `%cd:~,%` so that `%VAR%` is not expanded (std's rule). A refused
/// argument is an `Err` naming its place and the reason.
pub(super) fn batch_arguments(args: &[String]) -> Result<String, String> {
    let mut line = String::new();
    for (index, arg) in args.iter().enumerate() {
        if let Some(why) = refusal(arg) {
            return Err(format!("argument {} {why}", index + 1));
        }
        line.push(' ');
        append_batch_argument(&mut line, arg);
    }
    Ok(line)
}

fn append_batch_argument(line: &mut String, arg: &str) {
    // The characters that need no quotes; the rest of ASCII does, and so do the control characters. A `\` is fine
    // unless the argument is quoted anyway.
    const UNQUOTED: &str = r"#$*+-./:?@\_";
    let quote = arg.is_empty()
        || arg.ends_with('\\')
        || arg
            .chars()
            .any(|c| (c.is_ascii() && !(c.is_ascii_alphanumeric() || UNQUOTED.contains(c))) || c.is_control());
    if quote {
        line.push('"');
    }
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                // 2n backslashes before an inner quote, which a second quote then escapes.
                line.extend(std::iter::repeat_n('\\', backslashes));
                line.push('"');
            } else if c == '%' {
                line.push_str("%%cd:~,");
            }
            backslashes = 0;
        }
        line.push(c);
    }
    if quote {
        line.extend(std::iter::repeat_n('\\', backslashes));
        line.push('"');
    }
}

/// How `cmd.exe` is given the command of `cmd [switches] /c <command>`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CmdCommand {
    /// The arguments of `cmd.exe`: the switches, `/c`, and the command in one pair of quotes, which `cmd /c` strips.
    Line(String),
    /// A command of several lines, which a command line cannot carry: it runs from a temporary `.cmd` file, as a batch
    /// file does. `switches` is what comes before the file (the switches and `/c`, quoted) and `text` the lines, each
    /// ending with LF.
    Script { switches: String, text: String },
}

/// The way `cmd.exe` is given the command for `argv` when `argv` is a `/c` command (`cmd /d /c <command>`), or `None`
/// when it is not. One word after `/c` is cmd's own command text, which a script of the recipe is, and cmd reads it
/// as it is; several are a program and its arguments, each quoted as a batch file's argument is (std's rule), so that
/// every word stays one. A text of several lines is a [`CmdCommand::Script`]; a line break in any other word is
/// refused. `argv[0]` is the program.
pub(super) fn command_line_arguments(argv: &[String]) -> Option<Result<CmdCommand, String>> {
    let at = argv.iter().skip(1).position(|arg| arg.eq_ignore_ascii_case("/c"))? + 1;
    let words = &argv[at + 1..];
    if words.is_empty() {
        return None;
    }
    let mut switches = String::new();
    for (index, switch) in argv[1..at].iter().enumerate() {
        if let Some(why) = refusal(switch) {
            return Some(Err(format!("argument {} {why}", index + 1)));
        }
        append_batch_argument(&mut switches, switch);
        switches.push(' ');
    }
    switches.push_str(&argv[at]);
    if let [text] = words {
        if text.contains('\0') {
            return Some(Err(format!("argument {} contains a NUL", at + 1)));
        }
        if text.contains(['\r', '\n']) {
            return Some(Ok(CmdCommand::Script {
                switches,
                text: text.replace("\r\n", "\n").replace('\r', "\n"),
            }));
        }
        return Some(Ok(CmdCommand::Line(format!("{switches} \"{text}\""))));
    }
    let mut command = String::new();
    for (index, word) in words.iter().enumerate() {
        if let Some(why) = refusal(word) {
            return Some(Err(format!("argument {} {why}", at + 1 + index)));
        }
        if index > 0 {
            command.push(' ');
        }
        append_batch_argument(&mut command, word);
    }
    Some(Ok(CmdCommand::Line(format!("{switches} \"{command}\""))))
}
