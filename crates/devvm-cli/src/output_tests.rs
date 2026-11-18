use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use devvm::exit::{ExitStatus, ExitWidth};
use devvm::progress::{FileLog, LineProgress, Look, Progress};
use devvm::store::layers::SystemClock;
use devvm::{Dirs, Error, ErrorKind};

use crate::output::{Facts, Screen, choose, compose, end_with};

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn dirs_under(home: &Path) -> Dirs {
    Dirs::from_lookup(
        Some(home.to_path_buf()),
        &|name| match name {
            "XDG_STATE_HOME" => Some(home.join("state").into()),
            _ => None,
        },
        &|| Ok(home.join("run")),
    )
    .unwrap()
}

fn screen(buffer: &Buffer) -> Arc<dyn Progress> {
    Arc::new(LineProgress::to(Box::new(buffer.clone()), Look::default()))
}

#[skuld::test]
fn choose_returns_lines_unless_terminal_with_a_known_nonzero_width_and_no_dumb() {
    let facts = |is_terminal, width, dumb| Facts {
        is_terminal,
        width,
        dumb,
    };
    let table = [
        (facts(false, Some(100), false), Screen::Lines, "not a terminal"),
        (
            facts(true, None, false),
            Screen::Lines,
            "a terminal whose width cannot be read",
        ),
        (facts(true, Some(0), false), Screen::Lines, "a width of 0"),
        (facts(true, Some(100), true), Screen::Lines, "TERM=dumb"),
        (facts(true, Some(100), false), Screen::Tty, "a terminal with a width"),
    ];
    for (given, expected, what) in table {
        assert_eq!(choose(&given), expected, "{what}");
    }
}

#[skuld::test]
fn an_unwritable_log_dir_gives_a_warning_and_a_working_sink() {
    let tmp = skuld::TempDir::new().unwrap();
    let dirs = dirs_under(tmp.path());
    // `<state>/devvm` is a file, so `<state>/devvm/logs` cannot be made.
    std::fs::create_dir_all(dirs.state.parent().unwrap()).unwrap();
    std::fs::write(&dirs.state, "x").unwrap();
    let out = Buffer::default();

    let sinks = compose(screen(&out), Some(&dirs));
    sinks.progress.message("still works");

    assert!(sinks.log.is_none());
    let text = out.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(
        lines[0].starts_with("warning: cannot create the log directory "),
        "{text}"
    );
    assert_eq!(lines[1], "still works");
}

#[skuld::test]
fn a_writable_log_dir_gives_a_log_that_gets_what_the_screen_gets() {
    let tmp = skuld::TempDir::new().unwrap();
    let dirs = dirs_under(tmp.path());
    let out = Buffer::default();

    let sinks = compose(screen(&out), Some(&dirs));
    sinks.progress.message("hello");

    assert_eq!(out.text(), "hello\n");
    let log = sinks.log.expect("a log");
    assert_eq!(log.path().parent(), Some(dirs.logs().as_path()));
    assert!(std::fs::read_to_string(log.path()).unwrap().ends_with(" INFO hello\n"));
}

fn log_in(dir: &Path) -> FileLog {
    FileLog::create(dir, Arc::new(SystemClock)).unwrap()
}

fn ended(log: Option<&FileLog>, outcome: devvm::Result<ExitStatus>, colour: bool) -> (i32, String) {
    let mut stderr = Vec::new();
    let code = end_with(log, colour, None, outcome, ExitWidth::Full32, &mut stderr);
    (code, String::from_utf8(stderr).unwrap())
}

#[skuld::test]
fn a_failure_writes_the_log_line_then_the_error_line_and_records_it() {
    let tmp = skuld::TempDir::new().unwrap();
    let log = log_in(&tmp.path().join("logs"));

    let (code, stderr) = ended(Some(&log), Err(Error::new(ErrorKind::Io, "bad")), false);

    assert_eq!(code, 125);
    assert_eq!(stderr, format!("log: {}\ndevvm-error: io: bad\n", log.path().display()));
    let text = std::fs::read_to_string(log.path()).unwrap();
    assert!(
        text.lines().last().unwrap().ends_with(" ERROR devvm-error: io: bad"),
        "{text}"
    );
    assert_eq!(text.matches(" ERROR ").count(), 1, "{text}");
}

#[skuld::test]
fn a_success_prints_and_records_nothing() {
    let tmp = skuld::TempDir::new().unwrap();
    let log = log_in(&tmp.path().join("logs"));

    let (code, stderr) = ended(Some(&log), Ok(ExitStatus::Code(0)), false);
    let (guest_code, guest_stderr) = ended(Some(&log), Ok(ExitStatus::Code(7)), false);

    assert_eq!((code, stderr.as_str()), (0, ""));
    assert_eq!((guest_code, guest_stderr.as_str()), (7, ""));
    assert!(!std::fs::read_to_string(log.path()).unwrap().contains(" ERROR "));
}

#[skuld::test]
fn no_log_means_no_log_line() {
    let (_, stderr) = ended(None, Err(Error::new(ErrorKind::Io, "bad")), false);
    assert_eq!(stderr, "devvm-error: io: bad\n");
}

#[skuld::test]
fn colour_reaches_the_error_line_and_not_the_log_line_or_the_log() {
    let tmp = skuld::TempDir::new().unwrap();
    let log = log_in(&tmp.path().join("logs"));

    let (_, stderr) = ended(Some(&log), Err(Error::new(ErrorKind::Io, "bad")), true);

    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines[0], format!("log: {}", log.path().display()));
    assert_eq!(lines[1], "\u{1b}[31mdevvm-error:\u{1b}[0m io: bad");
    assert!(!std::fs::read_to_string(log.path()).unwrap().contains('\u{1b}'));
}
