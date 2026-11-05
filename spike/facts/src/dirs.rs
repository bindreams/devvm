//! Directories: renaming and deleting them while something has files inside open, renaming onto a name, the error
//! kinds of `remove_dir`, and two deleters of one tree.
use std::any::Any;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::sync::Barrier;

use crate::out::{err, row, show, show_with};
use crate::util::{Share, open_dir, open_read, touch};

pub fn run(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    open_inside(root);
    rename_onto(root);
    remove_dir_kinds(root);
    concurrent(root);
}

type Opener = fn(&Path) -> io::Result<Box<dyn Any>>;

fn file(share: Share, nested: bool, d: &Path) -> io::Result<Box<dyn Any>> {
    let path = if nested { d.join("sub").join("f") } else { d.join("f") };
    Ok(Box::new(open_read(&path, share)?))
}

/// Renaming and deleting a directory while something is open inside or on it.
fn open_inside(root: &Path) {
    let variants: [(&str, Opener); 9] = [
        ("file inside, share all", |d| file(Share::All, false, d)),
        ("file inside, share read+write (no delete)", |d| file(Share::NoDelete, false, d)),
        ("file inside, share none", |d| file(Share::Nothing, false, d)),
        ("file in a subdirectory, share all", |d| file(Share::All, true, d)),
        ("file in a subdirectory, share read+write", |d| file(Share::NoDelete, true, d)),
        ("the directory itself open, share all", |d| Ok(Box::new(open_dir(d, Share::All)?))),
        ("the directory itself open, share read+write", |d| {
            Ok(Box::new(open_dir(d, Share::NoDelete)?))
        }),
        ("the directory is the working directory", |d| {
            std::env::set_current_dir(d)?;
            Ok(Box::new(Cwd))
        }),
        ("a subdirectory is the working directory", |d| {
            std::env::set_current_dir(d.join("sub"))?;
            Ok(Box::new(Cwd))
        }),
    ];
    for (name, open) in variants {
        for op in ["rename", "remove_dir_all"] {
            let scratch = root.join("open-inside");
            let _ = std::fs::remove_dir_all(&scratch);
            let d = scratch.join("d");
            std::fs::create_dir_all(d.join("sub")).unwrap();
            touch(&d.join("f")).unwrap();
            touch(&d.join("sub").join("f")).unwrap();
            let held = open(&d);
            let held_text = show(&held);
            let moved = scratch.join("moved");
            let result = match op {
                "rename" => show(&std::fs::rename(&d, &moved)),
                _ => show(&std::fs::remove_dir_all(&d)),
            };
            let after = format!(
                "d_exists={} moved_exists={} d_symlink_metadata={}",
                d.exists(),
                moved.exists(),
                show(&std::fs::symlink_metadata(&d))
            );
            drop(held);
            let _ = std::env::set_current_dir(root);
            let again = match op {
                "rename" if d.exists() => show(&std::fs::rename(&d, &moved)),
                "remove_dir_all" if d.exists() => show(&std::fs::remove_dir_all(&d)),
                _ => "-".to_owned(),
            };
            row(
                "dir-open-inside",
                format!("{name} | open={held_text} | {op}={result} | {after} | {op} again after release={again}"),
            );
            let _ = std::fs::remove_dir_all(&scratch);
        }
    }
}

struct Cwd;

fn tree_summary(p: &Path) -> String {
    match std::fs::symlink_metadata(p) {
        Err(e) => err(&e),
        Ok(m) if m.is_dir() => format!(
            "dir with {} entries",
            std::fs::read_dir(p).map(|d| d.count()).unwrap_or(usize::MAX)
        ),
        Ok(m) => format!("{} of {} bytes", crate::out::kind_of(&m), m.len()),
    }
}

/// `rename(from, to)` onto each kind of target.
fn rename_onto(root: &Path) {
    let scratch = root.join("onto");
    for source in ["dir-with-file", "file"] {
        for target in ["absent", "empty-dir", "non-empty-dir", "file"] {
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::create_dir_all(&scratch).unwrap();
            let (from, to) = (scratch.join("from"), scratch.join("to"));
            if source == "file" {
                std::fs::write(&from, b"source").unwrap();
            } else {
                std::fs::create_dir(&from).unwrap();
                std::fs::write(from.join("in"), b"source").unwrap();
            }
            match target {
                "absent" => {}
                "empty-dir" => std::fs::create_dir(&to).unwrap(),
                "non-empty-dir" => {
                    std::fs::create_dir(&to).unwrap();
                    std::fs::write(to.join("theirs"), b"theirs").unwrap();
                }
                _ => std::fs::write(&to, b"target").unwrap(),
            }
            let result = std::fs::rename(&from, &to);
            row(
                "rename-onto",
                format!(
                    "source={source} target={target} -> {} | from now: {} | to now: {}",
                    show(&result),
                    tree_summary(&from),
                    tree_summary(&to)
                ),
            );
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
}

/// The error kinds of `remove_dir` and `remove_file` on what is not an empty directory / not a file.
fn remove_dir_kinds(root: &Path) {
    let scratch = root.join("kinds");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let non_empty = scratch.join("non-empty");
    std::fs::create_dir(&non_empty).unwrap();
    touch(&non_empty.join("f")).unwrap();
    let file = scratch.join("file");
    touch(&file).unwrap();
    let empty = scratch.join("empty");
    std::fs::create_dir(&empty).unwrap();
    let link_to_dir = scratch.join("link-to-dir");
    let linked = crate::cases::symlink(&empty, &link_to_dir, true);
    row("remove-dir", format!("remove_dir(non-empty dir) = {}", show(&std::fs::remove_dir(&non_empty))));
    row("remove-dir", format!("remove_dir(file) = {}", show(&std::fs::remove_dir(&file))));
    row("remove-dir", format!("remove_dir(missing) = {}", show(&std::fs::remove_dir(scratch.join("missing")))));
    row("remove-dir", format!("symlink to dir made: {}", show(&linked)));
    if linked.is_ok() {
        row(
            "remove-dir",
            format!(
                "remove_dir(symlink to empty dir) = {} ; target still there: {}",
                show(&std::fs::remove_dir(&link_to_dir)),
                empty.exists()
            ),
        );
    }
    row("remove-dir", format!("remove_file(dir) = {}", show(&std::fs::remove_file(&empty))));
    row("remove-dir", format!("remove_dir(empty dir) = {}", show(&std::fs::remove_dir(&empty))));
    let _ = std::fs::remove_dir_all(&scratch);
}

/// Two threads delete one tree; or one deletes while the other renames it away.
fn concurrent(root: &Path) {
    const TRIALS: usize = 300;
    for (name, second) in [("both remove_dir_all", Second::Remove), ("remove_dir_all vs rename", Second::Rename)] {
        let mut tally: BTreeMap<String, usize> = BTreeMap::new();
        let scratch = root.join("concurrent");
        for _ in 0..TRIALS {
            let _ = std::fs::remove_dir_all(&scratch);
            let tree = scratch.join("tree");
            for d in 0..4 {
                std::fs::create_dir_all(tree.join(format!("d{d}"))).unwrap();
                for f in 0..25 {
                    std::fs::write(tree.join(format!("d{d}")).join(format!("f{f}")), b"x").unwrap();
                }
            }
            let moved = scratch.join("moved");
            let barrier = Barrier::new(2);
            let (first, other) = std::thread::scope(|scope| {
                let a = scope.spawn(|| {
                    barrier.wait();
                    show(&std::fs::remove_dir_all(&tree))
                });
                let b = scope.spawn(|| {
                    barrier.wait();
                    match second {
                        Second::Remove => show(&std::fs::remove_dir_all(&tree)),
                        Second::Rename => show(&std::fs::rename(&tree, &moved)),
                    }
                });
                (a.join().unwrap(), b.join().unwrap())
            });
            let after = show_with(&std::fs::symlink_metadata(&tree), |_| "tree still there".to_owned());
            let moved_left = std::fs::symlink_metadata(&moved).is_ok();
            *tally
                .entry(format!("deleter={first} | other={other} | after: {after} | moved_exists={moved_left}"))
                .or_default() += 1;
        }
        let _ = std::fs::remove_dir_all(&scratch);
        for (outcome, count) in tally {
            row("concurrent-delete", format!("{name}, {TRIALS} trials: {count}x {outcome}"));
        }
    }
}

#[derive(Clone, Copy)]
enum Second {
    Remove,
    Rename,
}
