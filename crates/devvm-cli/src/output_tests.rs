use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use devvm::exit::{ExitStatus, ExitWidth};
use devvm::progress::{FileLog, LineProgress, Look, Progress};
use devvm::store::layers::SystemClock;
use devvm::{Dirs, Error, ErrorKind};

use crate::output::{Active, Facts, Run, Screen, Shown, choose, compose, end_with};

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

    let sinks = compose(screen(&out), Ok(&dirs));
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
fn a_state_directory_that_cannot_be_found_is_one_warning_and_a_working_sink() {
    let out = Buffer::default();
    let error = Error::new(ErrorKind::Io, "no state directory");

    let sinks = compose(screen(&out), Err(&error));
    sinks.progress.message("still works");

    assert!(sinks.log.is_none());
    assert_eq!(
        out.text(),
        "warning: no state directory
still works
"
    );
}

#[skuld::test]
fn a_writable_log_dir_gives_a_log_that_gets_what_the_screen_gets() {
    let tmp = skuld::TempDir::new().unwrap();
    let dirs = dirs_under(tmp.path());
    let out = Buffer::default();

    let sinks = compose(screen(&out), Ok(&dirs));
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

/// A status file that cannot be written: its directory is not there.
fn unwritable(tmp: &Path) -> std::path::PathBuf {
    tmp.join("missing").join("status.json")
}

fn ended_with_file(log: &FileLog, status_file: &Path, outcome: devvm::Result<ExitStatus>) -> (i32, String) {
    let mut stderr = Vec::new();
    let code = end_with(
        Some(log),
        false,
        Some(status_file),
        outcome,
        ExitWidth::Full32,
        &mut stderr,
    );
    (code, String::from_utf8(stderr).unwrap())
}

fn last_error_record(log: &FileLog) -> String {
    let text = std::fs::read_to_string(log.path()).unwrap();
    assert_eq!(text.matches(" ERROR ").count(), 1, "{text}");
    let record = text.lines().find(|line| line.contains(" ERROR ")).unwrap();
    record.split_once(" ERROR ").unwrap().1.to_owned()
}

#[skuld::test]
fn a_success_turned_into_a_failure_by_the_status_file_names_the_log_and_records_the_printed_line() {
    let tmp = skuld::TempDir::new().unwrap();
    let log = log_in(&tmp.path().join("logs"));

    let (code, stderr) = ended_with_file(&log, &unwritable(tmp.path()), Ok(ExitStatus::Code(0)));

    assert_eq!(code, 125);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    assert_eq!(lines[0], format!("log: {}", log.path().display()));
    assert!(lines[1].starts_with("devvm-error: status-file: "), "{stderr}");
    assert_eq!(last_error_record(&log), lines[1]);
}

#[skuld::test]
fn a_failure_and_an_unwritable_status_file_record_the_combined_line_that_was_printed() {
    let tmp = skuld::TempDir::new().unwrap();
    let log = log_in(&tmp.path().join("logs"));

    let (code, stderr) = ended_with_file(&log, &unwritable(tmp.path()), Err(Error::new(ErrorKind::Io, "bad")));

    assert_eq!(code, 125);
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    assert!(lines[1].contains("original: io: bad"), "{stderr}");
    assert_eq!(last_error_record(&log), lines[1]);
}

fn run_ending(run: &Run, outcome: devvm::Result<ExitStatus>) -> String {
    let mut stderr = Vec::new();
    run.end(false, None, outcome, ExitWidth::Full32, &mut stderr);
    String::from_utf8(stderr).unwrap()
}

fn failure() -> devvm::Result<ExitStatus> {
    Err(Error::new(ErrorKind::Io, "bad"))
}

#[skuld::test]
fn each_run_ends_with_its_own_log_and_leaves_nothing_for_the_next() {
    let tmp = skuld::TempDir::new().unwrap();
    let run = Run::new();
    let first = Arc::new(log_in(&tmp.path().join("first")));
    let second = Arc::new(log_in(&tmp.path().join("second")));

    run.begin(Active {
        log: Some(first.clone()),
        screen: None,
    });
    assert!(run_ending(&run, failure()).contains(&first.path().display().to_string()));

    run.begin(Active {
        log: Some(second.clone()),
        screen: None,
    });
    let text = run_ending(&run, failure());
    assert!(text.contains(&second.path().display().to_string()), "{text}");
    assert!(!text.contains(&first.path().display().to_string()), "{text}");

    // A run that never began has no log to name.
    assert_eq!(run_ending(&run, failure()), "devvm-error: io: bad\n");
}

// Who ends the run ====================================================================================================

/// What a panic hook prints for `message`.
fn panic_hook(run: &Run, message: &str) -> String {
    let mut stderr = Vec::new();
    let ended = run.on_panic(message.to_owned(), false, None, ExitWidth::Full32, &mut stderr);
    assert_eq!(ended.code, 125);
    String::from_utf8(stderr).unwrap()
}

#[skuld::test]
fn a_panic_with_no_ending_under_way_ends_the_run_and_names_the_log() {
    let tmp = skuld::TempDir::new().unwrap();
    let run = Run::new();
    let log = Arc::new(log_in(&tmp.path().join("logs")));
    run.begin(Active {
        log: Some(log.clone()),
        screen: None,
    });

    let text = panic_hook(&run, "boom");

    assert_eq!(
        text,
        format!("log: {}\ndevvm-error: panic: boom\n", log.path().display())
    );
    assert!(
        std::fs::read_to_string(log.path())
            .unwrap()
            .contains(" ERROR devvm-error: panic: boom")
    );
}

#[skuld::test]
fn a_panic_writes_the_status_file() {
    let tmp = skuld::TempDir::new().unwrap();
    let status = tmp.path().join("status.json");
    let mut stderr = Vec::new();

    let ended = Run::new().on_panic("boom".to_owned(), false, Some(&status), ExitWidth::Full32, &mut stderr);

    assert_eq!(ended.code, 125);
    let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&status).unwrap()).unwrap();
    assert_eq!(json["devvm"]["kind"], "panic");
    assert_eq!(json["devvm"]["message"], "boom");
}

/// The ending itself failed: nothing else would report it. The one-ending lock is the thread's own, so the hook is not
/// kept waiting for it.
#[skuld::test]
fn a_panic_on_the_endings_own_thread_ends_the_run() {
    let run = Run::new();
    let _ended = run.end(false, None, Ok(ExitStatus::Code(0)), ExitWidth::Full32, &mut Vec::new());

    assert_eq!(panic_hook(&run, "boom"), "devvm-error: panic: boom\n");
}

#[skuld::test]
fn each_ending_leaves_the_run_empty_for_the_next_panic() {
    let tmp = skuld::TempDir::new().unwrap();
    let run = Run::new();
    let log = Arc::new(log_in(&tmp.path().join("logs")));
    run.begin(Active {
        log: Some(log),
        screen: None,
    });
    assert!(run_ending(&run, failure()).contains("log: "));

    assert_eq!(panic_hook(&run, "boom"), "devvm-error: panic: boom\n");
}

// The line screen =====================================================================================================

fn run_with_lines(out: &Buffer) -> (Run, Arc<LineProgress>) {
    let lines = Arc::new(LineProgress::to(Box::new(out.clone()), Look::default()));
    let run = Run::new();
    run.begin(Active {
        log: None,
        screen: Some(Shown::Lines(lines.clone())),
    });
    (run, lines)
}

#[skuld::test]
fn the_ending_stops_the_line_screen_before_it_writes_the_error() {
    let out = Buffer::default();
    let (run, lines) = run_with_lines(&out);
    lines.message("before");

    run.end(false, None, failure(), ExitWidth::Full32, &mut out.clone());
    lines.message("late");
    lines.warn("late");

    assert_eq!(
        out.text(),
        "before
devvm-error: io: bad
"
    );
}

#[skuld::test]
fn the_ending_stops_the_log_before_it_records_the_error() {
    let tmp = skuld::TempDir::new().unwrap();
    let run = Run::new();
    let log = Arc::new(log_in(&tmp.path().join("logs")));
    run.begin(Active {
        log: Some(log.clone()),
        screen: None,
    });
    log.message("before");

    run_ending(&run, failure());
    log.message("late");

    let text = std::fs::read_to_string(log.path()).unwrap();
    assert!(!text.contains("late"), "{text}");
    assert!(
        text.lines().last().unwrap().ends_with(" ERROR devvm-error: io: bad"),
        "{text}"
    );
}

// The ending's own failure ============================================================================================

/// A writer that runs the panic hook on its first write, as a panic inside the ending's own write would.
struct PanicsOnFirstWrite<'a> {
    run: &'a Run,
    hook_text: Option<String>,
    first: bool,
}

impl Write for PanicsOnFirstWrite<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if std::mem::take(&mut self.first) {
            self.hook_text = Some(panic_hook(self.run, "boom"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The run is still there when the ending writes, so the hook of a panic on the ending's thread names the log and
/// records the error in it.
#[skuld::test]
fn a_panic_while_the_ending_writes_finds_the_log_of_the_run() {
    let tmp = skuld::TempDir::new().unwrap();
    let run = Run::new();
    let log = Arc::new(log_in(&tmp.path().join("logs")));
    run.begin(Active {
        log: Some(log.clone()),
        screen: None,
    });
    let mut stderr = PanicsOnFirstWrite {
        run: &run,
        hook_text: None,
        first: true,
    };

    run.end(false, None, failure(), ExitWidth::Full32, &mut stderr);

    let hook_text = stderr.hook_text.expect("the hook ended the run");
    assert_eq!(
        hook_text,
        format!(
            "log: {}
devvm-error: panic: boom
",
            log.path().display()
        )
    );
    let text = std::fs::read_to_string(log.path()).unwrap();
    assert!(text.contains(" ERROR devvm-error: panic: boom"), "{text}");
}

// A panic inside a sink ===============================================================================================
// The thread that panics is inside the sink's write, holding its lock, when the hook runs on it: the hook takes no
// sink's lock.

/// What a thread runs inside its next write or clock read, as the panic hook does when the thread panics there.
type Inside = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

fn run_inside(inside: &Inside) {
    let hook = inside.lock().unwrap().take();
    if let Some(hook) = hook {
        hook();
    }
}

/// A writer that runs a closure inside its first write, and panics out of it: the write is the one that fails.
struct Panicking {
    inside: Inside,
    written: Buffer,
}

impl Write for Panicking {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.inside.lock().unwrap().is_some() {
            run_inside(&self.inside);
            panic!("the write failed");
        }
        self.written.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct PanickingClock(Inside);

impl devvm::store::layers::Clock for PanickingClock {
    fn now(&self) -> std::time::SystemTime {
        if self.0.lock().unwrap().is_some() {
            run_inside(&self.0);
            panic!("the clock failed");
        }
        std::time::SystemTime::now()
    }
}

#[skuld::test]
fn a_hook_inside_a_line_write_ends_the_run_without_the_screens_lock() {
    let out = Buffer::default();
    let inside = Inside::default();
    let lines = Arc::new(LineProgress::to(
        Box::new(Panicking {
            inside: inside.clone(),
            written: out.clone(),
        }),
        Look::default(),
    ));
    let run = Arc::new(Run::new());
    run.begin(Active {
        log: None,
        screen: Some(Shown::Lines(lines.clone())),
    });
    let (hook_run, hook_out) = (run.clone(), out.clone());
    *inside.lock().unwrap() = Some(Box::new(move || {
        let mut stderr = hook_out.clone();
        let ended = hook_run.on_panic("boom".to_owned(), false, None, ExitWidth::Full32, &mut stderr);
        assert_eq!(ended.code, 125);
    }));

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lines.message("in the write")));

    assert!(unwound.is_err());
    assert_eq!(out.text(), "devvm-error: panic: boom\n");
}

/// The hook's record is written even though its thread is inside the log's lock.
#[skuld::test]
fn a_hook_inside_a_log_record_records_the_error() {
    let tmp = skuld::TempDir::new().unwrap();
    let inside = Inside::default();
    let log = Arc::new(FileLog::create(&tmp.path().join("logs"), Arc::new(PanickingClock(inside.clone()))).unwrap());
    let run = Arc::new(Run::new());
    run.begin(Active {
        log: Some(log.clone()),
        screen: None,
    });
    let hook_run = run.clone();
    let text = Arc::new(Mutex::new(None));
    let recorded = text.clone();
    *inside.lock().unwrap() = Some(Box::new(move || {
        *recorded.lock().unwrap() = Some(panic_hook(&hook_run, "boom"));
    }));

    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| log.message("in the record")));

    assert!(unwound.is_err());
    assert_eq!(
        text.lock().unwrap().as_deref(),
        Some(format!("log: {}\ndevvm-error: panic: boom\n", log.path().display()).as_str())
    );
    assert!(
        std::fs::read_to_string(log.path())
            .unwrap()
            .ends_with(" ERROR devvm-error: panic: boom\n")
    );
}
