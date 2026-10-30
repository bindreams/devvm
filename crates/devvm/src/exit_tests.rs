use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::exit::{
    DEVVM_ERROR_EXIT, ExitStatus, ExitWidth, Report, finish, format_error_line, status_json, write_status_file,
};
use crate::test_support::fixtures::unicode_home;
use crate::{Error, ErrorKind};

const WIDTHS: [ExitWidth; 2] = [ExitWidth::Full32, ExitWidth::LowByte];

fn code(width: ExitWidth, c: i32) -> i32 {
    width.exit_code(&ExitStatus::Code(c))
}

fn signal(name: &str) -> ExitStatus {
    ExitStatus::Signal {
        name: name.to_owned(),
        core_dumped: false,
    }
}

#[skuld::test]
fn low_byte_width_never_reports_a_false_success_or_a_false_devvm_failure() {
    for c in [256, 512, 125, 381, 0xFFFF_FF7Du32 as i32] {
        assert_eq!(code(ExitWidth::LowByte, c), 1, "{c}");
    }
    for c in [1, 7, 126, 255] {
        assert_eq!(code(ExitWidth::LowByte, c), c);
    }
    assert_eq!(code(ExitWidth::LowByte, 0), 0);
}

#[skuld::test]
fn full32_width_passes_every_code_through() {
    for c in [0, 125, 256, -1, 0xC000_0005u32 as i32] {
        assert_eq!(code(ExitWidth::Full32, c), c);
    }
}

#[skuld::test]
fn signal_exit_is_128_plus_number_under_both_widths() {
    for width in WIDTHS {
        assert_eq!(width.exit_code(&signal("SEGV")), 139);
        assert_eq!(width.exit_code(&signal("KILL")), 137);
        assert_eq!(width.exit_code(&signal("TRAP")), 133);
        assert_eq!(width.exit_code(&signal("BUS")), 135);
        assert_eq!(width.exit_code(&signal("FOO@x.org")), 1);
    }
}

fn parsed(report: &Report<'_>) -> Value {
    serde_json::from_str(&status_json(report)).unwrap()
}

#[skuld::test]
fn status_json_shapes_match_the_contract() {
    assert_eq!(
        status_json(&Report::Guest(&ExitStatus::Code(7))),
        r#"{"guest":{"code":7}}"#
    );
    assert_eq!(
        parsed(&Report::Guest(&ExitStatus::Code(-1))),
        json!({"guest": {"code": -1}})
    );
    assert_eq!(
        parsed(&Report::Guest(&signal("SEGV"))),
        json!({"guest": {"signal": "SEGV", "core_dumped": false}})
    );
    let error = Error::new(ErrorKind::NotFound, "no \"such\"\nfile");
    assert_eq!(
        parsed(&Report::Devvm(&error)),
        json!({"devvm": {"kind": "not-found", "message": "no \"such\"\nfile"}})
    );
}

#[skuld::test]
fn error_line_is_one_line(#[fixture(unicode_home)] home: &Path) {
    let message = "a\nb\r\nc\td\u{1b}e";
    let error = Error::new(ErrorKind::Io, message);
    let line = format_error_line(&error);
    assert!(line.starts_with("devvm-error: io: "), "{line}");
    assert!(!line.chars().any(char::is_control), "{line:?}");
    assert_eq!(line, r"devvm-error: io: a\nb\r\nc\td\u{1b}e");

    let path = home.join("status.json");
    write_status_file(&path, &Report::Devvm(&error)).unwrap();
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["devvm"]["message"], message);
}

fn run_finish(status_file: Option<&Path>, outcome: crate::Result<ExitStatus>, width: ExitWidth) -> (i32, String) {
    let mut stderr = Vec::new();
    let exit = finish(status_file, outcome, width, &mut stderr);
    (exit, String::from_utf8(stderr).unwrap())
}

#[skuld::test]
fn finish_writes_status_file_and_returns_125_for_devvm_errors(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("s.json");
    for width in WIDTHS {
        let (exit, stderr) = run_finish(Some(&path), Err(Error::new(ErrorKind::Checksum, "bad")), width);
        assert_eq!(exit, DEVVM_ERROR_EXIT);
        assert_eq!(stderr, "devvm-error: checksum: bad\n");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value, json!({"devvm": {"kind": "checksum", "message": "bad"}}));
    }
}

#[skuld::test]
fn finish_returns_guest_code_for_guest_status(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("s.json");
    let (exit, stderr) = run_finish(Some(&path), Ok(ExitStatus::Code(0x1_0000)), ExitWidth::LowByte);
    assert_eq!((exit, stderr.as_str()), (1, ""));
    let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value, json!({"guest": {"code": 0x1_0000}}));
    assert_eq!(
        run_finish(None, Ok(ExitStatus::Code(0x1_0000)), ExitWidth::Full32).0,
        0x1_0000
    );
}

#[skuld::test]
fn unwritable_status_file_gives_125_and_one_line(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("missing dir").join("s.json");
    let (exit, stderr) = run_finish(Some(&path), Ok(ExitStatus::Code(0)), ExitWidth::LowByte);
    assert_eq!(exit, DEVVM_ERROR_EXIT);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.starts_with("devvm-error: status-file: cannot write "),
        "{stderr}"
    );
    assert!(stderr.contains("missing dir"), "{stderr}");
    assert!(!stderr.contains("original"), "{stderr}");

    let (exit, stderr) = run_finish(
        Some(&path),
        Err(Error::new(ErrorKind::Usage, "bad flag")),
        ExitWidth::Full32,
    );
    assert_eq!(exit, DEVVM_ERROR_EXIT);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(stderr.ends_with("; original: usage: bad flag\n"), "{stderr}");
}

#[skuld::test]
fn status_file_replace_is_atomic_for_readers(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("s.json");
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Set on every exit of the writer, a panic included, so the reader below always ends.
            struct SetOnDrop<'a>(&'a AtomicBool);
            impl Drop for SetOnDrop<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::Release);
                }
            }
            let _done = SetOnDrop(&done);
            let error = Error::new(ErrorKind::Io, "x");
            for i in 0..200 {
                let guest = ExitStatus::Code(i);
                let report = if i % 2 == 0 {
                    Report::Guest(&guest)
                } else {
                    Report::Devvm(&error)
                };
                write_status_file(&path, &report).unwrap();
            }
        });
        while !done.load(Ordering::Acquire) {
            match std::fs::read_to_string(&path) {
                Ok(text) => {
                    assert!(!text.is_empty());
                    serde_json::from_str::<Value>(&text).unwrap();
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("{e}"),
            }
        }
    });
    let names: Vec<_> = std::fs::read_dir(home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["s.json"]);
}
