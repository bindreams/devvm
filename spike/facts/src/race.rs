//! What an observer sees while another thread creates and unlinks a file in a loop (the window between "being
//! unlinked" and "gone"), per call devvm makes on a lock file.
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::out::{row, show};

fn cycles() -> usize {
    std::env::var("SPIKE_CYCLES").ok().and_then(|v| v.parse().ok()).unwrap_or(30_000)
}

pub fn run(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    for keeper in [false, true] {
        observe(dir, keeper);
    }
}

fn observe(dir: &Path, keeper: bool) {
    let cycles = cycles();
    let path = dir.join("churn");
    let _ = fs::remove_file(&path);
    let done = AtomicBool::new(false);
    let mut tallies: BTreeMap<&str, BTreeMap<String, u64>> = BTreeMap::new();
    std::thread::scope(|scope| {
        let churn = scope.spawn(|| {
            for _ in 0..cycles {
                let created = OpenOptions::new().write(true).create(true).truncate(false).open(&path);
                let Ok(created) = created else { continue };
                let held = keeper.then(|| fs::File::open(&path));
                drop(created);
                let _ = fs::remove_file(&path);
                drop(held);
            }
            done.store(true, Ordering::Release);
        });
        while !done.load(Ordering::Acquire) {
            let samples: [(&str, String); 5] = [
                ("symlink_metadata", show(&fs::symlink_metadata(&path))),
                ("exists", path.exists().to_string()),
                ("try_exists", show(&path.try_exists())),
                ("open_rw_existing", show(&OpenOptions::new().read(true).write(true).open(&path))),
                ("open_read", show(&fs::File::open(&path))),
            ];
            for (call, outcome) in samples {
                *tallies.entry(call).or_default().entry(outcome).or_default() += 1;
            }
        }
        churn.join().unwrap();
    });
    let _ = fs::remove_file(&path);
    for (call, outcomes) in tallies {
        let text: Vec<String> = outcomes.iter().map(|(o, n)| format!("{n}x {o}")).collect();
        row(
            "unlink-window",
            format!("churn={cycles} cycles keeper_handle_open_at_unlink={keeper} {call}: {}", text.join(" ; ")),
        );
    }
}
