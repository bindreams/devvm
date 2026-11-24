use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::{Error, ErrorKind};
use crate::exit::format_error_line;
use crate::host::Host;
use crate::progress::log::{FileLog, keep_copy, keep_copy_with, owner_of, prune_one};
use crate::progress::{Event, Progress, Task, Total};
use crate::store::layers::Clock;
use crate::test_support::file_mode;
use crate::test_support::fixtures::unicode_home;
use crate::test_support::host::TestHost;

const DAY: u64 = 24 * 3600;

/// 2026-11-18T12:00:00Z.
fn noon() -> SystemTime {
    "2026-11-18T12:00:00Z".parse::<jiff::Timestamp>().unwrap().into()
}

struct FixedClock(SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

fn open(dir: &Path) -> FileLog {
    FileLog::create(dir, Arc::new(FixedClock(noon()))).unwrap()
}

/// A log made where the processes in `alive` are the running ones.
fn open_where(dir: &Path, alive: &[u32]) -> FileLog {
    FileLog::create_with(dir, Arc::new(FixedClock(noon())), &|pid| alive.contains(&pid)).unwrap()
}

/// The records of the log after its first (the header), as text.
fn records(log: &FileLog) -> Vec<String> {
    let text = std::fs::read_to_string(log.path()).unwrap();
    text.lines().skip(1).map(str::to_owned).collect()
}

const AT: &str = "2026-11-18T12:00:00.000Z";

#[skuld::test]
fn two_logs_in_one_directory_have_different_names(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    let (first, second) = (open(&dir), open(&dir));
    assert_ne!(first.path(), second.path());
    for log in [&first, &second] {
        let name = log.path().file_name().unwrap().to_str().unwrap();
        let rest = name
            .strip_prefix("devvm-20261118T120000Z-")
            .and_then(|rest| rest.strip_suffix(".log"))
            .unwrap_or_else(|| panic!("{name}"));
        let (pid, random) = rest.split_once('-').unwrap();
        assert_eq!(pid, std::process::id().to_string());
        assert!(
            random.len() == 4 && random.chars().all(|c| c.is_ascii_hexdigit()),
            "{name}"
        );
        assert_eq!(log.path().parent(), Some(dir.as_path()));
    }
}

#[skuld::test]
fn a_log_holds_every_event_with_a_timestamp(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    let progress: &dyn Progress = &log;
    progress.message("hello");
    progress.warn("careful");
    progress.debug("detail");
    progress.output("a step line");
    let outer = progress.task("outer", Total::Items(2));
    let task = progress.task("hashing a.iso", Total::Bytes(100));
    task.status("halfway");
    task.set(50);
    let (outer_id, id) = (outer.id(), task.id());
    task.finish();
    drop(outer);
    assert_eq!(
        records(&log),
        [
            format!("{AT} INFO hello"),
            format!("{AT} WARN careful"),
            format!("{AT} DEBUG detail"),
            format!("{AT} OUTPUT a step line"),
            format!("{AT} TASK {outer_id} begin parent=- items=2 outer"),
            format!("{AT} TASK {id} begin parent={outer_id} bytes=100 hashing a.iso"),
            format!("{AT} TASK {id} status halfway"),
            format!("{AT} TASK {id} advance 50"),
            format!("{AT} TASK {id} end finished"),
            format!("{AT} TASK {outer_id} end abandoned"),
        ]
    );
}

#[skuld::test]
fn a_5_gib_task_logs_twenty_advances(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    let progress: &dyn Progress = &log;
    let size = 5u64 << 30;
    let task = progress.task("hashing big.iso", Total::Bytes(size));
    for mib in 1..=size >> 20 {
        task.set(mib << 20);
    }
    task.finish();
    let advances: Vec<String> = records(&log)
        .into_iter()
        .filter(|line| line.contains(" advance "))
        .collect();
    assert_eq!(advances.len(), 20, "{advances:?}");
    assert!(
        advances[0].ends_with(&format!(" advance {}", size / 20)),
        "{}",
        advances[0]
    );
    assert!(advances[19].ends_with(&format!(" advance {size}")), "{}", advances[19]);
}

#[skuld::test]
fn output_lines_are_complete_and_controls_are_escaped(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    let long = "x".repeat(100_000);
    log.output(&long);
    log.event(&Event::Output {
        line: "redraw\r10%\u{1b}[31m red\0 nul".to_owned(),
    });
    log.warn("two\nlines");
    let records = records(&log);
    assert_eq!(records.len(), 3, "{records:?}");
    assert_eq!(records[0], format!("{AT} OUTPUT {long}"));
    assert_eq!(records[1], format!("{AT} OUTPUT redraw\\r10%\\u{{1b}}[31m red\\0 nul"));
    assert_eq!(records[2], format!("{AT} WARN two\\nlines"));
}

fn age(path: &Path, days: u64) {
    let seconds = noon().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() - days * DAY;
    TestHost.set_mtime(path, u32::try_from(seconds).unwrap()).unwrap();
}

#[skuld::test]
fn files_older_than_14_days_are_removed_and_newer_ones_kept(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let put = |name: &str, days: u64| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "x").unwrap();
        age(&path, days);
        path
    };
    let old = put("devvm-20261101T000000Z-1-aaaa.log", 15);
    let recent = put("devvm-20261105T000000Z-1-bbbb.log", 13);
    // Not a log of ours, however old.
    let other = put("notes.txt", 40);
    let log = open_where(&dir, &[]);
    assert!(!old.exists(), "a log of 15 days is removed");
    assert!(recent.exists() && other.exists());
    assert!(log.path().exists());
}

#[skuld::test]
fn an_unwritable_directory_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("not a directory");
    std::fs::write(&file, "x").unwrap();
    let dir = file.join("logs");
    let error = FileLog::create(&dir, Arc::new(FixedClock(noon()))).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains(&dir.display().to_string()), "{error}");
}

#[skuld::test]
fn a_record_reaches_the_file_before_the_next_event(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    log.message("first");
    assert_eq!(records(&log), [format!("{AT} INFO first")]);
    log.message("second");
    assert_eq!(records(&log).len(), 2);
}

#[skuld::test]
fn the_log_file_is_the_users_alone_whatever_the_directory_hands_down(#[fixture(unicode_home)] home: &Path) {
    let loose = home.join("loose state");
    std::fs::create_dir(&loose).unwrap();
    #[cfg(windows)]
    let me = crate::test_support::private_files::let_other_accounts_write_below(&loose);
    let log = open(&loose.join("logs"));
    log.message("after the reopen");
    assert_eq!(file_mode::mode(log.path()), cfg!(unix).then_some(0o600));
    #[cfg(windows)]
    crate::test_support::private_files::assert_is_the_users_alone(log.path(), &me);
}

#[skuld::test]
fn failed_writes_one_error_record_with_controls_escaped(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    log.failed(&format_error_line(&Error::new(ErrorKind::Io, "bad\nthing\u{1b}[0m")));
    assert_eq!(
        records(&log),
        [format!("{AT} ERROR devvm-error: io: bad\\nthing\\u{{1b}}[0m")]
    );
}

#[skuld::test]
fn a_begin_whose_parent_was_never_seen_is_logged_with_that_parent_id(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    let elsewhere = crate::progress::NullProgress;
    let outer = Task::begin(&elsewhere, "outer", Total::Unknown);
    let inner = Task::begin(&log, "inner", Total::Unknown);
    let (outer_id, inner_id) = (outer.id(), inner.id());
    drop(inner);
    drop(outer);
    assert_eq!(
        records(&log),
        [
            format!("{AT} TASK {inner_id} begin parent={outer_id} unknown inner"),
            format!("{AT} TASK {inner_id} end abandoned"),
        ]
    );
}

/// A clock that runs one closure when it is first read, which is inside the log's lock.
#[derive(Default)]
struct HookClock(std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl Clock for HookClock {
    fn now(&self) -> SystemTime {
        let hook = self.0.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        noon()
    }
}

/// A panic inside the log's lock reaches the panic hook on the same thread, whose record needs no lock: it is written.
#[skuld::test]
fn a_record_made_by_a_hook_on_the_thread_inside_the_log_is_written(#[fixture(unicode_home)] home: &Path) {
    let clock = Arc::new(HookClock::default());
    let log = Arc::new(FileLog::create(&home.join("logs"), clock.clone()).unwrap());
    let again = log.clone();
    *clock.0.lock().unwrap() = Some(Box::new(move || again.failed("from the hook")));

    log.warn("outer");
    log.failed("later");

    assert_eq!(
        records(&log),
        [
            format!("{AT} ERROR from the hook"),
            format!("{AT} WARN outer"),
            format!("{AT} ERROR later")
        ]
    );
}

#[skuld::test]
fn a_stopped_log_records_no_event_and_still_takes_the_error(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    log.message("before");
    log.stop();
    log.message("after");
    log.failed("the end");
    assert_eq!(
        records(&log),
        [format!("{AT} INFO before"), format!("{AT} ERROR the end")]
    );
}

/// A file that cannot be written: the disk is full.
struct FullDisk;

impl std::io::Write for FullDisk {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("the disk is full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[skuld::test]
fn a_log_that_could_not_be_written_says_so_with_the_first_error(#[fixture(unicode_home)] home: &Path) {
    let log = open(&home.join("logs"));
    assert_eq!(log.describe(), log.path().display().to_string());

    log.state.lock().unwrap().out = Box::new(FullDisk);
    log.warn("first");
    log.state.lock().unwrap().out = Box::new(std::io::sink());
    log.warn("second");

    assert_eq!(
        log.describe(),
        format!("{} (incomplete: the disk is full)", log.path().display())
    );
}

#[skuld::test]
fn a_log_whose_process_is_running_is_kept_however_old_and_the_others_are_removed(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let put = |name: &str| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "x").unwrap();
        age(&path, 60);
        path
    };
    let running = put("devvm-20260101T000000Z-7-aaaa.log");
    let gone = put("devvm-20260101T000000Z-8-bbbb.log");
    let nameless = put("devvm-old.log");

    let _next = open_where(&dir, &[7]);

    assert!(running.exists(), "the process 7 is running");
    assert!(!gone.exists(), "the process 8 is not");
    assert!(
        !nameless.exists(),
        "a log with no pid in its name is removed by its age"
    );
}

/// The check the command line uses, on the pid of this very process.
#[skuld::test]
fn the_log_of_this_process_is_not_pruned_however_old(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    let mine = open(&dir);
    let path = mine.path().to_owned();
    age(&path, 15);

    let _next = open(&dir);

    assert!(path.exists());
}

#[skuld::test]
fn the_owner_is_the_pid_before_the_last_dash() {
    assert_eq!(owner_of("devvm-20260101T000000Z-7-aaaa.log"), Some(7));
    assert_eq!(owner_of("devvm-2026-01-01-7-aaaa.log"), Some(7));
    assert_eq!(owner_of("devvm-old.log"), None);
}

// Kept copies ---------------------------------------------------------------------------------------------------------

/// A file of `bytes` in `home`, to be copied.
fn source_of(home: &Path, bytes: &[u8]) -> PathBuf {
    let path = home.join("serial.log");
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A copy of `source` into `dir`, where the warnings are collected in `warnings`.
fn copy_of(dir: &Path, source: &Path, warnings: &mut Vec<String>) -> std::io::Result<PathBuf> {
    keep_copy(dir, &FixedClock(noon()), source, &mut |warning| {
        warnings.push(warning.to_owned())
    })
}

#[skuld::test]
fn a_kept_copy_has_the_content_and_a_serial_log_name(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    let content: &[u8] = b"boot\r\nlogin:\n";
    let source = source_of(home, content);
    let mut warnings = Vec::new();
    let copy = copy_of(&dir, &source, &mut warnings).unwrap();
    assert_eq!(std::fs::read(&copy).unwrap(), content);
    assert_eq!(copy.parent(), Some(dir.as_path()), "the directory is made");
    let name = copy.file_name().unwrap().to_str().unwrap();
    let rest = name
        .strip_prefix("devvm-20261118T120000Z-")
        .and_then(|rest| rest.strip_suffix(".serial.log"))
        .unwrap_or_else(|| panic!("{name}"));
    let (pid, random) = rest.split_once('-').unwrap();
    assert_eq!(pid, std::process::id().to_string());
    assert!(
        random.len() == 4 && random.chars().all(|c| c.is_ascii_hexdigit()),
        "{name}"
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(source.exists(), "the original stays");
}

#[skuld::test]
fn a_copy_of_a_large_file_is_complete(#[fixture(unicode_home)] home: &Path) {
    let bytes: Vec<u8> = (0..6u32 << 20).map(|n| (n % 251) as u8).collect();
    let source = source_of(home, &bytes);
    let copy = copy_of(&home.join("logs"), &source, &mut Vec::new()).unwrap();
    assert!(
        std::fs::read(&copy).unwrap() == bytes,
        "the copy differs from the {} byte original",
        bytes.len()
    );
}

#[skuld::test]
fn two_kept_copies_of_one_file_have_different_names(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    let source = source_of(home, b"x");
    let (first, second) = (
        copy_of(&dir, &source, &mut Vec::new()).unwrap(),
        copy_of(&dir, &source, &mut Vec::new()).unwrap(),
    );
    assert_ne!(first, second);
    assert!(first.exists() && second.exists());
}

#[skuld::test]
fn a_kept_copy_is_the_users_alone(#[fixture(unicode_home)] home: &Path) {
    let loose = home.join("loose state");
    std::fs::create_dir(&loose).unwrap();
    #[cfg(windows)]
    let me = crate::test_support::private_files::let_other_accounts_write_below(&loose);
    let source = source_of(home, b"secret");
    let copy = copy_of(&loose.join("logs"), &source, &mut Vec::new()).unwrap();
    assert_eq!(file_mode::mode(&copy), cfg!(unix).then_some(0o600));
    #[cfg(windows)]
    crate::test_support::private_files::assert_is_the_users_alone(&copy, &me);
}

#[skuld::test]
fn owner_of_a_kept_copy_is_this_process(#[fixture(unicode_home)] home: &Path) {
    let source = source_of(home, b"x");
    let copy = copy_of(&home.join("logs"), &source, &mut Vec::new()).unwrap();
    assert_eq!(
        owner_of(copy.file_name().unwrap().to_str().unwrap()),
        Some(std::process::id())
    );
}

#[skuld::test]
fn keeping_a_copy_prunes_old_copies_and_the_old_logs_of_processes_that_are_gone(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let put = |name: &str, days: u64| -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "x").unwrap();
        age(&path, days);
        path
    };
    let running_log = put("devvm-20260101T000000Z-7-aaaa.log", 60);
    let running_copy = put("devvm-20260101T000000Z-7-bbbb.serial.log", 60);
    let gone_log = put("devvm-20260101T000000Z-8-cccc.log", 60);
    let gone_copy = put("devvm-20260101T000000Z-8-dddd.serial.log", 60);
    let recent_copy = put("devvm-20261110T000000Z-8-eeee.serial.log", 5);
    let source = source_of(home, b"x");

    let copy = keep_copy_with(&dir, &FixedClock(noon()), &source, &mut |_| {}, &|pid| pid == 7).unwrap();

    assert!(
        running_log.exists(),
        "the process 7 is running, and may still write its log"
    );
    assert!(
        !running_copy.exists() && !gone_log.exists() && !gone_copy.exists(),
        "a copy is complete when made, whoever made it; the process 8 is gone"
    );
    assert!(recent_copy.exists() && copy.exists());
}

#[skuld::test]
fn a_log_that_is_gone_when_its_age_is_asked_is_not_a_warning() {
    let path = Path::new("devvm-20260101T000000Z-8-aaaa.log");
    let mut warnings = Vec::new();
    prune_one(
        path,
        "devvm-20260101T000000Z-8-aaaa.log",
        Err(std::io::ErrorKind::NotFound.into()),
        noon(),
        &|_| false,
        &mut warnings,
    );
    assert!(warnings.is_empty(), "{warnings:?}");

    prune_one(
        path,
        "devvm-20260101T000000Z-8-aaaa.log",
        Err(std::io::ErrorKind::PermissionDenied.into()),
        noon(),
        &|_| false,
        &mut warnings,
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("cannot tell the age"), "{warnings:?}");
}

/// A file another process holds open without sharing deletion cannot be removed by the prune.
#[cfg(windows)]
#[skuld::test]
fn a_prune_warning_goes_to_warn_and_the_copy_is_still_made(#[fixture(unicode_home)] home: &Path) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let dir = home.join("logs");
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("devvm-20260101T000000Z-8-aaaa.log");
    std::fs::write(&old, "x").unwrap();
    age(&old, 60);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&old)
        .unwrap();
    let source = source_of(home, b"kept");

    let mut warnings = Vec::new();
    let copy = keep_copy_with(
        &dir,
        &FixedClock(noon()),
        &source,
        &mut |warning| warnings.push(warning.to_owned()),
        &|_| false,
    );
    drop(held);

    let copy = copy.unwrap();
    assert_eq!(std::fs::read(copy).unwrap(), b"kept");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("cannot remove the old log") && warnings[0].contains("aaaa"),
        "{warnings:?}"
    );
}

#[skuld::test]
fn a_source_that_is_not_there_is_an_error_naming_it_and_makes_no_file(#[fixture(unicode_home)] home: &Path) {
    let dir = home.join("logs");
    let source = home.join("absent.log");
    let error = copy_of(&dir, &source, &mut Vec::new()).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "{error}");
    assert!(error.to_string().contains(&source.display().to_string()), "{error}");
    assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
}
