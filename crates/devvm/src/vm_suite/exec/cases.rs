//! The exec suite's case table: what a command run in a guest must do, whichever way it is run.
//!
//! Both test binaries include this file by `#[path]` and supply one [`ExecTarget`] each: the lib runs the cases through
//! `Vm::run`, `devvm-cli` through the real `devvm ssh <name> --`. A case is a function of the target; the binary
//! registers a test that calls it. A case takes its programs from the target's [`Os`]. It uses only `std`, because the
//! two binaries share nothing else.

#![allow(dead_code, reason = "each including test binary runs part of the table")]

/// A command to run in the guest.
pub struct Case {
    pub argv: Vec<String>,
    pub elevated: bool,
    pub cwd: Option<String>,
}

impl Case {
    pub fn new(argv: &[&str]) -> Case {
        Case {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            elevated: false,
            cwd: None,
        }
    }

    pub fn elevated(mut self) -> Case {
        self.elevated = true;
        self
    }

    pub fn in_dir(mut self, cwd: &str) -> Case {
        self.cwd = Some(cwd.to_owned());
        self
    }
}

/// How a guest command ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    Code(i32),
    Signal(String),
}

/// What running a [`Case`] gave. `stderr` is the guest's, and may hold devvm's own progress lines too, so a case
/// looks into it only for what it knows the guest wrote.
pub struct Observed {
    pub ended: Ended,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Observed {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// The guest OS a case runs on: its programs are the guest's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

/// The way a binary runs a case. A target fails the test, with a message that names what went wrong, when devvm itself
/// fails the command (a `devvm-error`) or the target cannot express the case.
pub trait ExecTarget {
    /// The OS of the guest the target runs in.
    fn os(&self) -> Os;

    fn run(&self, case: &Case) -> Observed;
}

// Programs ============================================================================================================

/// A PowerShell 7 command of a Windows guest: `script` is the whole `-Command` text.
fn pwsh(script: &str) -> Case {
    Case::new(&["pwsh", "-NoProfile", "-NonInteractive", "-Command", script])
}

/// Where the Windows table keeps the script that prints its arguments ([`ARGV_ECHO`]). The directory is the one the
/// base's bootstrap makes for the user to write.
const ARGV_SCRIPT: &str = r"C:\ProgramData\devvm\argv-echo.ps1";

/// A script that prints each argument it gets as raw UTF-8 bytes followed by a NUL: the Windows table's `printf '%s\0'`.
/// A `-File` script gets the arguments as the command line holds them, which `-Command` does not.
const ARGV_ECHO: &str = "$out = [Console]::OpenStandardOutput(); foreach ($a in $args) { \
    $b = [Text.Encoding]::UTF8.GetBytes($a); $out.Write($b, 0, $b.Length); $out.WriteByte(0) }; $out.Flush()";

// Cases ===============================================================================================================

/// A guest exit code reaches the host as it is, 125 and 255 included. A Windows guest adds the NTSTATUS codes of a
/// crashed program and -1: the host gets the same 32 bits, the analogue of a signal there.
pub fn exit_7_125_255(target: &dyn ExecTarget) {
    let codes: &[i32] = match target.os() {
        Os::Linux => &[7, 125, 255],
        // 0xC0000005 (access violation), 0xC000013A (Ctrl-C) and 0xFFFFFFFF.
        Os::Windows => &[7, 125, 255, 0xC000_0005_u32 as i32, 0xC000_013A_u32 as i32, -1],
    };
    for &code in codes {
        let case = match target.os() {
            Os::Linux => Case::new(&["sh", "-c", &format!("exit {code}")]),
            Os::Windows => pwsh(&format!("exit {code}")),
        };
        let seen = target.run(&case);
        assert_eq!(seen.ended, Ended::Code(code), "exit {code}: {}", seen.stderr_text());
    }
}

/// The line devvm prints for its own failures, written by the guest, is output, and 125 is the guest's status.
pub fn a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(target: &dyn ExecTarget) {
    let case = match target.os() {
        Os::Linux => Case::new(&["sh", "-c", "echo 'devvm-error: x: y'; exit 125"]),
        Os::Windows => pwsh("[Console]::Out.Write('devvm-error: x: y' + [char]10); exit 125"),
    };
    let seen = target.run(&case);
    assert_eq!(seen.ended, Ended::Code(125), "{}", seen.stderr_text());
    assert_eq!(seen.stdout_text(), "devvm-error: x: y\n");
}

/// A command killed by SIGSEGV ends as signal 11, which a process exit code makes 139 (128 and the signal's number).
/// sshd reports the signal when the command is the shell's last, and bash's own 139 otherwise. Linux only: a Windows
/// guest has no signals, and `exit_7_125_255` runs its analogue.
pub fn kill_minus_segv_dollar_dollar_exits_139(target: &dyn ExecTarget) {
    assert_eq!(target.os(), Os::Linux, "only a Linux guest has signals");
    let seen = target.run(&Case::new(&["sh", "-c", "kill -SEGV $$"]));
    assert!(
        matches!(&seen.ended, Ended::Signal(name) if name == "SEGV") || seen.ended == Ended::Code(139),
        "{:?}: {}",
        seen.ended,
        seen.stderr_text()
    );
}

/// The arguments a host passes arrive in the guest as they are, and an argument that looks like an option or an
/// assignment is a program name when it is the first.
pub fn argv_round_trips_hostile_arguments(target: &dyn ExecTarget) {
    let mut hostile = vec![
        "",
        "a\nb",
        "it's",
        "\"quoted\"",
        "$x",
        "`id`",
        "%",
        "^",
        "&",
        ";",
        "\u{e9} \u{416} \u{4e16} \u{1f600}",
        "-x",
        "a=b",
        "  lead and trail  ",
        "\\n",
    ];
    let mut argv: Vec<&str> = match target.os() {
        Os::Linux => vec!["printf", r"%s\0"],
        Os::Windows => {
            // What quoting a command line gets wrong: a backslash before a quote or at the end, and the parser's own
            // marker.
            hostile.extend([r"trail\", r#"a\"b"#, r"C:\dir with space\", "--%", "\u{2018}x\u{2019}"]);
            let write = format!(
                "[IO.File]::WriteAllText('{ARGV_SCRIPT}', '{}')",
                ARGV_ECHO.replace('\'', "''")
            );
            let seen = target.run(&pwsh(&write));
            assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
            vec!["pwsh", "-NoProfile", "-NonInteractive", "-File", ARGV_SCRIPT]
        }
    };
    argv.extend(&hostile);
    let seen = target.run(&Case::new(&argv));
    assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
    let mut got: Vec<&[u8]> = seen.stdout.split(|byte| *byte == 0).collect();
    assert_eq!(got.pop(), Some(&b""[..]), "every argument ends with a NUL");
    let want: Vec<&[u8]> = hostile.iter().map(|arg| arg.as_bytes()).collect();
    assert_eq!(got, want);

    for name in ["-x", "a=b"] {
        let seen = target.run(&Case::new(&[name]));
        // 127 is "no such program": an option would give 2, and an assignment would leave `exec` with no program.
        assert_eq!(seen.ended, Ended::Code(127), "{name}: {}", seen.stderr_text());
        assert!(seen.stderr_text().contains(name), "{name}: {}", seen.stderr_text());
    }
}

/// What a command that prints its working directory gives when it starts in `dir`.
fn where_am_i(target: &dyn ExecTarget, dir: &str, elevated: bool) -> Observed {
    let case = match target.os() {
        Os::Linux => Case::new(&["pwd"]),
        // The process's own directory, which is what a program the command starts inherits.
        Os::Windows => pwsh("[Console]::Out.Write([Environment]::CurrentDirectory)"),
    }
    .in_dir(dir);
    target.run(&if elevated { case.elevated() } else { case })
}

/// The working directory is where the command starts. The command line has no flag for one, so only a library target
/// runs this.
pub fn cwd_is_honored(target: &dyn ExecTarget) {
    let (dir, printed) = match target.os() {
        Os::Linux => ("/tmp", "/tmp\n"),
        Os::Windows => (r"C:\ProgramData\devvm", r"C:\ProgramData\devvm"),
    };
    let seen = where_am_i(target, dir, false);
    assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
    assert_eq!(seen.stdout_text(), printed);
}

/// An elevated command enters a directory only root can enter, because the `cd` runs in the root shell. Library only,
/// like [`cwd_is_honored`]. On a Windows guest that is a directory the user's Medium token cannot enter.
pub fn elevated_cwd_is_honored(target: &dyn ExecTarget) {
    let (dir, printed) = match target.os() {
        Os::Linux => ("/root", "/root\n"),
        Os::Windows => (r"C:\Windows\System32\config", r"C:\Windows\System32\config"),
    };
    let seen = where_am_i(target, dir, true);
    assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
    assert_eq!(seen.stdout_text(), printed);
}

/// An elevated command is root. On a Windows guest it runs at High integrity (`S-1-16-12288`: a SID, never a localized
/// name).
pub fn elevated_runs_as_root(target: &dyn ExecTarget) {
    match target.os() {
        Os::Linux => {
            let seen = target.run(&Case::new(&["id", "-u"]).elevated());
            assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
            assert_eq!(seen.stdout_text(), "0\n");
        }
        Os::Windows => {
            let seen = target.run(&pwsh("whoami /groups").elevated());
            assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
            assert!(seen.stdout_text().contains("S-1-16-12288"), "{}", seen.stdout_text());
        }
    }
}
