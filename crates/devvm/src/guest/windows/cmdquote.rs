//! How `cmd.exe` reads the arguments of a batch file: a port of Rust std's rules for running a `.bat` or `.cmd` (the fix
//! of CVE-2024-24576), `append_bat_arg` and `make_bat_command_line` in `library/std/src/sys/args/windows.rs`. It is the
//! one copy in this repository: cosca's `quote::windows` quotes for `CommandLineToArgvW` and its `batch_gate` refuses
//! batch targets, and ssh-broker's shim packs one command for a shell, so none of them quotes for `cmd.exe`.
//!
//! The arguments std refuses are refused: a `\r` or a `\n`, which ends a cmd.exe command line, so a command of several
//! lines has no command line. A NUL never gets here: `wrap` refuses it in every argument first.

#[cfg(test)]
#[path = "cmdquote_tests.rs"]
mod cmdquote_tests;

/// Why `cmd.exe` cannot be given `arg`, if it cannot.
fn refusal(arg: &str) -> Option<&'static str> {
    debug_assert!(!arg.contains('\0'), "a NUL is refused before the quoting");
    arg.contains(['\r', '\n'])
        .then_some("contains a line break, which ends a cmd.exe command line")
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

/// How `cmd.exe` reads one argument before its command text.
enum Switches {
    /// Switches that end in `/c`, as one word (`/c`, `/d/c`, `/e:on/c`): the command text follows.
    Carrier,
    /// Other switches, or text cmd reads as itself.
    Plain,
    /// A `/c` or `/k` that is not carried: stuck to other text (`/cdir`) or `/k`.
    Stuck,
}

/// Reads the switches of `arg`, one at a time as cmd does: a switch is `/` and a letter, and `/e:on` takes its value up
/// to the next `/`.
fn read_switches(arg: &str) -> Switches {
    let mut rest = arg;
    while let Some(after) = rest.strip_prefix('/') {
        let mut chars = after.chars();
        let Some(letter) = chars.next() else { break };
        let tail = chars.as_str();
        match letter.to_ascii_lowercase() {
            'c' | 'k' => {
                let plain = arg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | ':'));
                return if letter.eq_ignore_ascii_case(&'c') && tail.is_empty() && plain {
                    Switches::Carrier
                } else {
                    Switches::Stuck
                };
            }
            _ => rest = tail.find('/').map_or("", |at| &tail[at..]),
        }
    }
    Switches::Plain
}

/// Whether `program` names `cmd.exe`: `cmd` or `cmd.exe` in any case, after any directory. Another name for it (a
/// copy) is decided in the guest, by the file found.
fn names_cmd(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("cmd") || name.eq_ignore_ascii_case("cmd.exe")
}

/// The arguments of `cmd.exe` for `argv` when `argv[0]` names cmd and `argv` is a `/c` command (`cmd /d /c <command>`),
/// or `None` when it is not: the switches, `/c`, and the command in one pair of quotes, which `cmd /c` strips. The `/c`
/// is a word of its own or the end of a chain of switches (`/d/c`); a `/c` stuck to other text and a `/k` are an `Err`,
/// not carried. One word after `/c` is cmd's own command text, which a script of the recipe is, and cmd reads it as it
/// is; several are a program and its arguments, each quoted as a batch file's argument is (std's rule), so that every
/// word stays one. A word that cannot be given to cmd, a line break in the text included, is an `Err` naming its place,
/// the number of its argument in `argv`.
pub(super) fn command_line_arguments(argv: &[String]) -> Option<Result<String, String>> {
    if !names_cmd(&argv[0]) {
        return None;
    }
    let mut carrier = None;
    for (place, arg) in argv.iter().enumerate().skip(1) {
        match read_switches(arg) {
            Switches::Carrier => {
                carrier = Some(place);
                break;
            }
            Switches::Stuck => {
                return Some(Err(format!(
                    "argument {place} ({arg:?}) is a /c or /k that devvm does not carry to cmd.exe; write /c as an \
                     argument of its own, or ending a chain of switches such as /d/c"
                )));
            }
            Switches::Plain => {}
        }
    }
    let at = carrier?;
    let words = &argv[at + 1..];
    if words.is_empty() {
        return None;
    }
    let mut line = String::new();
    for (place, switch) in argv.iter().enumerate().take(at).skip(1) {
        if let Some(why) = refusal(switch) {
            return Some(Err(format!("argument {place} {why}")));
        }
        append_batch_argument(&mut line, switch);
        line.push(' ');
    }
    line.push_str(&argv[at]);
    line.push_str(" \"");
    let single = words.len() == 1;
    for (index, word) in words.iter().enumerate() {
        if let Some(why) = refusal(word) {
            return Some(Err(format!("argument {} {why}", at + 1 + index)));
        }
        if single {
            line.push_str(word);
        } else {
            if index > 0 {
                line.push(' ');
            }
            append_batch_argument(&mut line, word);
        }
    }
    line.push('"');
    Some(Ok(line))
}
