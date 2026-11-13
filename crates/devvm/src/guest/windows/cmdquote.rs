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

/// The arguments of `cmd.exe` for `argv` when it is a `/c` command (`cmd /d /c <script>`), or `None` when it is not:
/// the switches before `/c` as words, then `/c`, then everything after it, joined by spaces, in one pair of quotes,
/// which `cmd /c` strips (its rule for a command that starts with a quote and holds others). What follows `/c` is
/// cmd's own command text, so it is not quoted argument by argument. `argv[0]` is the program.
pub(super) fn command_line_arguments(argv: &[String]) -> Option<Result<String, String>> {
    let at = argv.iter().skip(1).position(|arg| arg.eq_ignore_ascii_case("/c"))? + 1;
    let text = &argv[at + 1..];
    if text.is_empty() {
        return None;
    }
    if let Some((index, why)) = argv
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, arg)| refusal(arg).map(|why| (index, why)))
    {
        return Some(Err(format!("argument {index} {why}")));
    }
    let mut line = String::new();
    for switch in &argv[1..at] {
        append_batch_argument(&mut line, switch);
        line.push(' ');
    }
    line.push_str(&argv[at]);
    line.push_str(" \"");
    line.push_str(&text.join(" "));
    line.push('"');
    Some(Ok(line))
}
