use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::{Error, ErrorKind};
use crate::host::Host;
use crate::progress::log::FileLog;
use crate::progress::{Progress, Task, Total};
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
    log.output("redraw\r10%\u{1b}[31m red\0 nul");
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
    let log = open(&dir);
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
    log.failed(&Error::new(ErrorKind::Io, "bad\nthing\u{1b}[0m"));
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
