//! Lock identity: what a holder of a lock on a file can tell once the name stops leading to its file, and whether two
//! holders of "the same" lock can exist. And file ids: stability and reuse.
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use crate::out::{err, row, show, show_with};
use crate::util::{Share, lock_text, open_dir, touch};

fn rw_create(p: &Path) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(true).truncate(false).open(p)
}

/// The id of the file `file` is open on.
pub fn id_of(file: &File) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(format!("{}:{}", m.dev(), m.ino()))
    }
    #[cfg(windows)]
    crate::win::file_id(file)
}

/// The id of whatever the name leads to, opened for attributes only.
pub fn id_by_name(path: &Path) -> io::Result<String> {
    id_of(&open_dir(path, Share::All)?)
}

/// What the holder `file` of a lock on `path` sees of itself and of the name.
fn view(file: &File, path: &Path) -> String {
    let mine = id_of(file);
    let name = id_by_name(path);
    let same = match (&mine, &name) {
        (Ok(a), Ok(b)) => (a == b).to_string(),
        _ => "-".to_owned(),
    };
    #[cfg(unix)]
    let extra = {
        use std::os::unix::fs::MetadataExt;
        format!("nlink={}", show_with(&file.metadata(), |m| m.nlink().to_string()))
    };
    #[cfg(windows)]
    let extra = crate::win::holder_view(file);
    format!(
        "holder_id={} by_name_id={} same={same} {extra}",
        show_with(&mine, Clone::clone),
        show_with(&name, Clone::clone)
    )
}

pub fn locks(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    type Op = fn(&Path) -> io::Result<()>;
    fn unlink(p: &Path) -> io::Result<()> {
        std::fs::remove_file(p)
    }
    fn rename_away(p: &Path) -> io::Result<()> {
        std::fs::rename(p, p.with_extension("moved"))
    }
    fn replace(p: &Path) -> io::Result<()> {
        let q = p.with_extension("new");
        touch(&q)?;
        std::fs::rename(&q, p)
    }
    let scenarios: [(&str, bool, Option<Op>, bool); 6] = [
        ("control-nothing-happens", true, None, true),
        ("unlink-then-recreate", true, Some(unlink), true),
        ("unlink-only", true, Some(unlink), false),
        ("rename-away-then-recreate", true, Some(rename_away), true),
        ("replace-by-rename", true, Some(replace), true),
        ("opened-then-unlinked-then-locked", false, Some(unlink), true),
    ];
    for (name, lock_first, op, recreate) in scenarios {
        let p = dir.join(format!("{name}.lock"));
        for suffix in ["", ".moved", ".new"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
        }
        let a = rw_create(&p).unwrap();
        let a_first = lock_first.then(|| lock_text(a.try_lock()));
        let done = op.map(|op| show(&op(&p)));
        let a_late = (!lock_first).then(|| lock_text(a.try_lock()));
        let b = recreate.then(|| rw_create(&p));
        let b_open = b.as_ref().map(show);
        let b_lock = b.as_ref().and_then(|b| b.as_ref().ok()).map(|b| lock_text(b.try_lock()));
        row(
            "lock-identity",
            format!(
                "scenario={name} a_lock_first={a_first:?} op={done:?} a_lock_late={a_late:?} b_open={b_open:?} \
                 b_lock={b_lock:?} :: A sees [{}]",
                view(&a, &p)
            ),
        );
    }
}

pub fn ids(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let (f1, f2, d) = (dir.join("f1"), dir.join("f2"), dir.join("d"));
    touch(&f1).unwrap();
    touch(&f2).unwrap();
    std::fs::create_dir_all(&d).unwrap();
    let a = open_dir(&f1, Share::All).unwrap();
    let id_a = id_of(&a);
    let id_b = id_by_name(&f1);
    row("file-id", format!("f1 first_open={} second_open={}", show_with(&id_a, Clone::clone), show_with(&id_b, Clone::clone)));
    row(
        "file-id",
        format!(
            "f1_vs_f2 distinct={:?} dir={}",
            id_a.as_ref().ok().zip(id_by_name(&f2).ok().as_ref()).map(|(x, y)| x != y),
            show_with(&id_by_name(&d), Clone::clone)
        ),
    );
    let moved = dir.join("f1-moved");
    let renamed = std::fs::rename(&f1, &moved);
    row(
        "file-id",
        format!(
            "after rename {} id_same_as_open_handle={:?} handle_still_valid={}",
            show(&renamed),
            id_by_name(&moved).ok().zip(id_a.as_ref().ok()).map(|(x, y)| x == *y),
            show_with(&id_of(&a), Clone::clone)
        ),
    );
    drop(a);
    reuse(dir, false);
    reuse(dir, true);
}

/// Creates and deletes in a loop: does a new file get the id of a deleted one?
fn reuse(dir: &Path, directories: bool) {
    const CYCLES: usize = 2000;
    let (x, y) = (dir.join("reuse-x"), dir.join("reuse-y"));
    let make = |p: &Path| -> io::Result<()> {
        if directories { std::fs::create_dir(p) } else { touch(p) }
    };
    let remove = |p: &Path| -> io::Result<()> {
        if directories { std::fs::remove_dir(p) } else { std::fs::remove_file(p) }
    };
    let mut seen: HashSet<String> = HashSet::new();
    let (mut adjacent, mut repeats, mut failures) = (0usize, 0usize, 0usize);
    let mut first_error = None;
    for _ in 0..CYCLES {
        let step = (|| -> io::Result<(String, String)> {
            make(&x)?;
            let id_x = id_by_name(&x)?;
            remove(&x)?;
            make(&y)?;
            let id_y = id_by_name(&y)?;
            remove(&y)?;
            Ok((id_x, id_y))
        })();
        match step {
            Ok((id_x, id_y)) => {
                adjacent += usize::from(id_x == id_y);
                repeats += usize::from(!seen.insert(id_x));
                repeats += usize::from(!seen.insert(id_y));
            }
            Err(e) => {
                failures += 1;
                first_error.get_or_insert_with(|| err(&e));
            }
        }
    }
    row(
        "file-id-reuse",
        format!(
            "kind={} cycles={CYCLES} (create x, id, delete, create y, id, delete) adjacent_same_id={adjacent} \
             ids_seen_again={repeats} distinct={} failures={failures} first_error={first_error:?}",
            if directories { "dir" } else { "file" },
            seen.len()
        ),
    );
}
