use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::mpsc;

use crate::exit::{NullProgress, Progress};
use crate::store::lock::{FileLock, Mode};
use crate::test_support::fixtures::anna_home;
use crate::test_support::helper;

/// Helper process: takes an exclusive lock on `args[0]`, prints `held`, then blocks until stdin closes.
pub fn hold_lock(args: &[String]) {
    let mut lock = FileLock::open(Path::new(&args[0])).unwrap();
    assert!(lock.try_lock(Mode::Exclusive).unwrap());
    println!("held");
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

#[skuld::test]
fn exclusive_excludes_exclusive_and_shared_in_one_process(#[fixture(anna_home)] home: &Path) {
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
fn shared_locks_coexist(#[fixture(anna_home)] home: &Path) {
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
fn contention_message_is_emitted_before_blocking(#[fixture(anna_home)] home: &Path) {
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
fn uncontended_lock_emits_no_message(#[fixture(anna_home)] home: &Path) {
    let (sender, receiver) = mpsc::channel();
    let progress = Capture(Mutex::new(sender));
    FileLock::open(&home.join("a.lock"))
        .unwrap()
        .lock(Mode::Exclusive, "x", &progress)
        .unwrap();
    drop(progress);
    assert!(receiver.recv().is_err());
}

#[skuld::test]
fn lock_is_released_when_holder_process_is_killed(#[fixture(anna_home)] home: &Path) {
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
