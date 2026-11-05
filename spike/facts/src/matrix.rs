//! Every call on every case, each on a fresh scratch directory.
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use crate::out::{err, row};
use crate::{calls, cases};

pub fn run(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    let cases = cases::all();
    let calls = calls::all();
    for (i, case) in cases.iter().enumerate() {
        let mut cells: Vec<String> = Vec::new();
        let mut note = String::new();
        let mut setup_failed = None;
        for call in &calls {
            let scratch = root.join(format!("c{i}-{}", call.name));
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                std::fs::create_dir_all(&scratch).unwrap();
                let made = (case.make)(&scratch);
                let mut made = match made {
                    Ok(made) => made,
                    Err(e) => return Err(err(&e)),
                };
                made.scratch = scratch.clone();
                let text = (call.run)(&mut made);
                if note.is_empty() {
                    note.clone_from(&made.note);
                }
                for restore in &made.restore {
                    restore.apply();
                }
                drop(made);
                Ok(text)
            }))
            .unwrap_or_else(|_| Ok("PANIC".to_owned()));
            let _ = std::fs::remove_dir_all(&scratch);
            match outcome {
                Ok(text) => cells.push(format!("{}={text}", call.name)),
                Err(why) => {
                    setup_failed = Some(why);
                    break;
                }
            }
        }
        match setup_failed {
            Some(why) => row("lookup", format!("case={} SETUP {why}", case.name)),
            None => row(
                "lookup",
                format!("case={} note=[{note}] :: {}", case.name, cells.join(" ; ")),
            ),
        }
    }
}
