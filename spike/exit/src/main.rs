//! Throwaway measurement for devvm#40. The lines starting `RESULT` are the evidence.
//!
//! - `u1 <normal|killed> <trials> <dir>` (every OS): a parent holds a lock the product uses (Windows: a share-mode-0
//!   file handle made inheritable; Unix: an flock'd file), a child inherits it, the parent drops its own copy and ends
//!   the child (it exits normally, or is killed). The parent then loops with no sleep: try to take the hold, then ask
//!   whether the child has exited. Counted: iterations where the hold was free while the child was not yet seen as
//!   exited (`window`), and where the child was seen as exited while the hold was still held (`reverse`).
//! - `u2 <dir>...` (Windows): `remove_file` on a file with a second handle open, then list and re-create the name.
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};

#[cfg(unix)]
#[path = "hold_unix.rs"]
mod hold;
#[cfg(windows)]
#[path = "hold_windows.rs"]
mod hold;
#[cfg(windows)]
mod u2;

#[derive(Clone, Copy)]
enum ExitKind {
    Normal,
    Killed,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("child") => child(),
        Some("u1") => {
            let kind = match args[2].as_str() {
                "normal" => ExitKind::Normal,
                "killed" => ExitKind::Killed,
                other => panic!("unknown exit kind {other:?}"),
            };
            u1(kind, args[3].parse().unwrap(), Path::new(&args[4]));
        }
        #[cfg(windows)]
        Some("u2") => u2::run(&args[2..]),
        other => panic!("unknown mode {other:?}"),
    }
}

/// The child holds the inherited hold for as long as it lives: it reports ready, then blocks until stdin closes.
fn child() {
    std::io::stdout().write_all(b"R").unwrap();
    std::io::stdout().flush().unwrap();
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

#[derive(Default)]
struct Counts {
    trials_with_window: u64,
    window_iterations: u64,
    trials_with_reverse: u64,
    reverse_iterations: u64,
    iterations: u64,
}

fn u1(kind: ExitKind, trials: u64, dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let lock_path = dir.join("hold");
    let mut counts = Counts::default();
    for _ in 0..trials {
        trial(kind, &lock_path, &mut counts);
    }
    println!(
        "RESULT u1 os={} exit={} trials={trials} trials_with_window={} window_iterations={} \
         trials_with_reverse={} reverse_iterations={} total_iterations={}",
        std::env::consts::OS,
        match kind {
            ExitKind::Normal => "normal",
            ExitKind::Killed => "killed",
        },
        counts.trials_with_window,
        counts.window_iterations,
        counts.trials_with_reverse,
        counts.reverse_iterations,
        counts.iterations,
    );
}

fn trial(kind: ExitKind, lock_path: &Path, counts: &mut Counts) {
    let hold = hold::Hold::create(lock_path);
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("child")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    hold.inherit_into(&mut command);
    let mut child = command.spawn().unwrap();

    // The child is up and holds its inherited copy.
    let mut ready = [0u8; 1];
    child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
    drop(hold);
    assert!(
        !hold::try_take(lock_path),
        "the child does not hold the inherited lock; the measurement is void"
    );

    // End the child, then race it.
    let stdin = child.stdin.take().unwrap();
    match kind {
        ExitKind::Normal => drop(stdin),
        ExitKind::Killed => {
            child.kill().unwrap();
            drop(stdin);
        }
    }
    race(&mut child, lock_path, counts);
    child.wait().unwrap();
}

/// Loops with no sleep until the child is seen as exited and the hold can be taken.
fn race(child: &mut Child, lock_path: &Path, counts: &mut Counts) {
    let (mut windows, mut reverses) = (0u64, 0u64);
    loop {
        let taken = hold::try_take(lock_path);
        let exited = hold::has_exited(child);
        counts.iterations += 1;
        if taken && !exited {
            windows += 1;
        }
        if exited && !taken {
            reverses += 1;
        }
        if exited && taken {
            break;
        }
    }
    counts.window_iterations += windows;
    counts.reverse_iterations += reverses;
    counts.trials_with_window += u64::from(windows > 0);
    counts.trials_with_reverse += u64::from(reverses > 0);
}
