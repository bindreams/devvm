//! The exec suite's case table: what a command run in a guest must do, whichever way it is run.
//!
//! Both test binaries include this file by `#[path]` and supply one [`ExecTarget`] each: the lib runs the cases through
//! `Vm::run`, `devvm-cli` through the real `devvm ssh <name> --`. A case is a function of the target; the binary
//! registers a test that calls it. It uses only `std`, because the two binaries share nothing else.

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

/// The way a binary runs a case. A target fails the test, with a message that names what went wrong, when devvm itself
/// fails the command (a `devvm-error`) or the target cannot express the case.
pub trait ExecTarget {
    fn run(&self, case: &Case) -> Observed;
}

// Cases ===============================================================================================================

/// A guest exit code reaches the host as it is, 125 and 255 included.
pub fn exit_7_125_255(target: &dyn ExecTarget) {
    for code in [7, 125, 255] {
        let seen = target.run(&Case::new(&["sh", "-c", &format!("exit {code}")]));
        assert_eq!(seen.ended, Ended::Code(code), "exit {code}: {}", seen.stderr_text());
    }
}

/// The line devvm prints for its own failures, written by the guest, is output, and 125 is the guest's status.
pub fn a_guest_printing_devvm_error_and_exiting_125_is_a_guest_status(target: &dyn ExecTarget) {
    let seen = target.run(&Case::new(&["sh", "-c", "echo 'devvm-error: x: y'; exit 125"]));
    assert_eq!(seen.ended, Ended::Code(125), "{}", seen.stderr_text());
    assert_eq!(seen.stdout_text(), "devvm-error: x: y\n");
}

/// A command killed by SIGSEGV ends as signal 11, which a process exit code makes 139 (128 and the signal's number).
/// sshd reports the signal when the command is the shell's last, and bash's own 139 otherwise.
pub fn kill_minus_segv_dollar_dollar_exits_139(target: &dyn ExecTarget) {
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
    let hostile = [
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
    let mut argv = vec!["printf", r"%s\0"];
    argv.extend(hostile);
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

/// The working directory is where the command starts. The command line has no flag for one, so only a library target
/// runs this.
pub fn cwd_is_honored(target: &dyn ExecTarget) {
    let seen = target.run(&Case::new(&["pwd"]).in_dir("/tmp"));
    assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
    assert_eq!(seen.stdout_text(), "/tmp\n");
}

/// An elevated command enters a directory only root can enter, because the `cd` runs in the root shell. Library only,
/// like [`cwd_is_honored`].
pub fn elevated_cwd_is_honored(target: &dyn ExecTarget) {
    let seen = target.run(&Case::new(&["pwd"]).in_dir("/root").elevated());
    assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
    assert_eq!(seen.stdout_text(), "/root\n");
}

/// An elevated command is root.
pub fn elevated_runs_as_root(target: &dyn ExecTarget) {
    let seen = target.run(&Case::new(&["id", "-u"]).elevated());
    assert_eq!(seen.ended, Ended::Code(0), "{}", seen.stderr_text());
    assert_eq!(seen.stdout_text(), "0\n");
}
