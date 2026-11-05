use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::exit::{NullProgress, Progress};
use crate::host::Comparison;
use crate::store::lock::{FileLock, Mode};
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::helper;
use crate::test_support::name_change::NameChange;
use crate::test_support::symlink::symlink;

/// Helper process: takes an exclusive lock on `args[0]`, prints `held`, then blocks until stdin closes.
pub fn hold_lock(args: &[String]) {
    let mut lock = FileLock::open(Path::new(&args[0])).unwrap();
    assert!(lock.try_lock(Mode::Exclusive).unwrap());
    println!("held");
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

#[skuld::test]
fn exclusive_excludes_exclusive_and_shared_in_one_process(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("locks").join("a.lock");
    let mut first = FileLock::open(&path).unwrap();
    let mut second = FileLock::open(&path).unwrap();
    assert!(first.try_lock(Mode::Exclusive).unwrap());
    assert!(!second.try_lock(Mode::Exclusive).unwrap());
    assert!(!second.try_lock(Mode::Shared).unwrap());
    first.unlock().unwrap();
    assert!(second.try_lock(Mode::Shared).unwrap());
    assert!(!first.try_lock(Mode::Exclusive).unwrap());
}

#[skuld::test]
fn shared_locks_coexist(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a.lock");
    let mut first = FileLock::open(&path).unwrap();
    let mut second = FileLock::open(&path).unwrap();
    assert!(first.try_lock(Mode::Shared).unwrap());
    assert!(second.try_lock(Mode::Shared).unwrap());
}

struct Capture(Mutex<mpsc::Sender<String>>);

impl Progress for Capture {
    fn message(&self, text: &str) {
        self.0.lock().unwrap().send(text.to_owned()).unwrap();
    }
}

#[skuld::test]
fn contention_message_is_emitted_before_blocking(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a.lock");
    let mut holder = FileLock::open(&path).unwrap();
    assert!(holder.try_lock(Mode::Exclusive).unwrap());
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        let waiter = scope.spawn(|| {
            let mut lock = FileLock::open(&path).unwrap();
            lock.lock(Mode::Exclusive, "layer K", &Capture(Mutex::new(sender)))
                .unwrap();
        });
        assert_eq!(receiver.recv().unwrap(), "waiting for layer K");
        assert!(!waiter.is_finished());
        holder.unlock().unwrap();
        waiter.join().unwrap();
    });
}

#[skuld::test]
fn uncontended_lock_emits_no_message(#[fixture(unicode_home)] home: &Path) {
    let (sender, receiver) = mpsc::channel();
    let progress = Capture(Mutex::new(sender));
    FileLock::open(&home.join("a.lock"))
        .unwrap()
        .lock(Mode::Exclusive, "x", &progress)
        .unwrap();
    drop(progress);
    assert!(receiver.recv().is_err());
}

/// `pause` runs between attempts; the first contention is said once, however many pauses follow.
#[skuld::test]
fn lock_while_says_it_waits_once_and_gives_up_when_the_pause_says_to_stop(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a.lock");
    let mut holder = FileLock::open(&path).unwrap();
    assert!(holder.try_lock(Mode::Exclusive).unwrap());
    let progress = MemoryProgress::default();
    let mut pauses = 0;

    let locked = FileLock::open(&path)
        .unwrap()
        .lock_while(
            Mode::Exclusive,
            "layer K",
            &progress,
            Some(&mut || {
                pauses += 1;
                Ok(pauses == 3)
            }),
        )
        .unwrap();

    assert!(!locked);
    assert_eq!(pauses, 3);
    assert_eq!(progress.messages(), ["waiting for layer K"]);
}

#[skuld::test]
fn lock_while_takes_the_lock_once_it_is_free(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a.lock");
    let mut holder = FileLock::open(&path).unwrap();
    assert!(holder.try_lock(Mode::Exclusive).unwrap());
    let mut lock = FileLock::open(&path).unwrap();

    let locked = lock
        .lock_while(
            Mode::Exclusive,
            "layer K",
            &NullProgress,
            Some(&mut || {
                holder.unlock()?;
                Ok(false)
            }),
        )
        .unwrap();

    assert!(locked);
}

#[skuld::test]
fn lock_is_released_when_holder_process_is_killed(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a.lock");
    let mut child = helper::command("hold-lock", &[path.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");

    let mut probe = FileLock::open(&path).unwrap();
    assert!(!probe.try_lock(Mode::Exclusive).unwrap());
    child.kill().unwrap();
    child.wait().unwrap();
    // Blocks until the OS releases the dead holder's lock; it has no cap because the release is the event awaited.
    probe.lock(Mode::Exclusive, "the killed holder", &NullProgress).unwrap();
}

#[skuld::test]
fn file_lock_is_send_and_sync() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<FileLock>();
}

/// Two threads sharing one `FileLock` could both "hold" it on Unix, where `flock` converts a lock the same open file
/// already has. Locking and unlocking take `&mut self`, so a shared reference cannot do either.
#[skuld::test]
fn locking_requires_exclusive_access_to_the_value() {
    let _: fn(&mut FileLock, Mode) -> crate::Result<bool> = FileLock::try_lock;
    let _: fn(&mut FileLock, Mode, &str, &dyn Progress) -> crate::Result<()> = FileLock::lock;
    let _: fn(&mut FileLock) -> crate::Result<()> = FileLock::unlock;
}

#[skuld::test]
fn a_blocking_lock_call_interrupted_by_a_signal_is_retried_until_it_succeeds() {
    let mut interruptions = 5;
    let result = super::retry_interrupted(|| {
        if interruptions > 0 {
            interruptions -= 1;
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
        }
        Ok(())
    });
    assert!(result.is_ok());
    assert_eq!(interruptions, 0);
}

#[skuld::test]
fn other_errors_are_not_retried() {
    let mut calls = 0;
    let result = super::retry_interrupted::<()>(|| {
        calls += 1;
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    });
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(calls, 1);
}

#[skuld::test]
fn open_existing_returns_none_for_a_missing_file_and_does_not_create_it(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("missing").join("a.lock");
    assert!(FileLock::open_existing(&path).unwrap().is_none());
    assert!(!path.exists());
    assert!(!path.parent().unwrap().exists());
}

#[skuld::test]
fn open_in_existing_dir_creates_the_file_and_not_its_directory(#[fixture(unicode_home)] home: &Path) {
    let made = home.join("a.lock");
    let mut lock = FileLock::open_in_existing_dir(&made).unwrap();
    assert!(made.exists());
    assert!(lock.try_lock(Mode::Exclusive).unwrap());

    let gone = home.join("missing").join("deeper").join("b.lock");
    let error = FileLock::open_in_existing_dir(&gone).unwrap_err();
    assert!(
        error.message().contains(&gone.parent().unwrap().display().to_string()),
        "{error}"
    );
    assert!(!home.join("missing").exists());
}

#[skuld::test]
fn open_existing_locks_like_open(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a.lock");
    drop(FileLock::open(&path).unwrap());
    let mut existing = FileLock::open_existing(&path).unwrap().expect("the file exists");
    assert!(existing.try_lock(Mode::Exclusive).unwrap());
    let mut other = FileLock::open(&path).unwrap();
    assert!(!other.try_lock(Mode::Exclusive).unwrap());
    assert!(!other.try_lock(Mode::Shared).unwrap());
}

#[skuld::test]
fn a_lock_is_the_file_at_its_path_until_the_name_leads_to_another_file(#[fixture(unicode_home)] home: &Path) {
    for (n, change) in NameChange::ALL.into_iter().enumerate() {
        let path = home.join(format!("a{n}.lock"));
        let lock = FileLock::open(&path).unwrap();
        assert_eq!(
            lock.is_at_path().unwrap(),
            Comparison::Same,
            "{change:?}: before the change"
        );
        change.apply(&path);
        assert!(path.exists(), "{change:?}: a file is at the name");
        assert_eq!(
            lock.is_at_path().unwrap(),
            Comparison::Different,
            "{change:?}: after the change"
        );
    }
}

#[skuld::test]
fn a_lock_is_absent_from_its_path_once_the_name_is_unlinked(#[fixture(unicode_home)] home: &Path) {
    let path = home.join("a.lock");
    let lock = FileLock::open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(lock.is_at_path().unwrap(), Comparison::Absent);
}

#[skuld::test]
fn a_link_at_the_lock_name_is_an_error_at_every_open(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let (target, dangling, link) = (home.join("target"), home.join("nowhere"), home.join("a.lock"));
    std::fs::write(&target, b"").unwrap();
    for leads_to in [&target, &dangling] {
        let _ = std::fs::remove_file(&link);
        symlink(leads_to, &link);
        let errors = [
            FileLock::open(&link).unwrap_err(),
            FileLock::open_in_existing_dir(&link).unwrap_err(),
            FileLock::open_existing(&link).map(drop).unwrap_err(),
        ];
        for error in errors {
            assert!(error.message().contains(&link.display().to_string()), "{error}");
            assert!(error.message().contains("link"), "{error}");
        }
    }
    assert!(!dangling.exists(), "no open made the target of a dangling link");
}

#[skuld::test]
fn open_existing_reads_every_absent_path_as_none(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("file");
    std::fs::write(&file, b"").unwrap();
    for path in [
        home.join("missing").join("a.lock"),
        file.join("a.lock"),
        home.join("a|b.lock"),
    ] {
        assert!(FileLock::open_existing(&path).unwrap().is_none(), "{}", path.display());
    }
}
