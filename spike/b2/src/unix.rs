//! Claims 1 and 2: PATH_MAX and NAME_MAX.

use crate::calls::{RmOrder, probe, row};
use crate::ladder::{Kind, Ladder, absent_path};
use std::fs;
use std::io;
use std::path::Path;

pub fn run(command: &str, label: &str, dir: &Path, _max_length: Option<usize>) -> io::Result<()> {
    match command {
        "long" => long(label, dir),
        "component" => component(label, dir),
        other => Err(io::Error::other(format!("unknown command {other}"))),
    }
}

/// Claim 1 and the PATH_MAX boundary of claim 2: paths of exact lengths around and past PATH_MAX, every component short.
fn long(label: &str, dir: &Path) -> io::Result<()> {
    let base = fs::canonicalize(dir)?;
    let root = base.join("L");
    let pm = libc::PATH_MAX as usize;
    row("env", label, 0, &[("PATH_MAX", pm.to_string()), ("base", base.display().to_string())]);
    let lengths: Vec<usize> = (pm - 3..=pm + 2).chain([pm + 100, 2 * pm]).collect();
    let jobs: Vec<(usize, Kind)> = lengths.iter().flat_map(|&n| [(n - 2, Kind::File), (n, Kind::Dir)]).collect();

    let mut ladder = Ladder::new(root.clone(), true, usize::MAX)?;
    let leaves = ladder.build(&jobs);
    std::env::set_current_dir(&base)?;

    let claim = "1-long";
    for n in lengths {
        let leaf = |target, kind| leaves.iter().find(|l| l.target == target && l.kind == kind).expect("leaf was planned");
        match &leaf(n - 2, Kind::File).path {
            Ok(dir) => {
                let file = dir.join("f");
                probe(claim, &format!("{label}:file-exists"), file.as_os_str().len(), &file, RmOrder::FileFirst, false);
                let absent = dir.join("g");
                probe(claim, &format!("{label}:file-absent"), absent.as_os_str().len(), &absent, RmOrder::FileFirst, false);
            }
            Err(why) => row(claim, &format!("{label}:file-exists"), n, &[("build", why.clone())]),
        }
        match &leaf(n, Kind::Dir).path {
            Ok(dir) => probe(claim, &format!("{label}:dir-exists"), dir.as_os_str().len(), dir, RmOrder::DirFirst, false),
            Err(why) => row(claim, &format!("{label}:dir-exists"), n, &[("build", why.clone())]),
        }
        let nothing = absent_path(&root, n);
        probe(claim, &format!("{label}:nothing-exists"), nothing.as_os_str().len(), &nothing, RmOrder::FileFirst, false);
    }
    row("cleanup", label, 0, &[("remove_dir_all", crate::calls::show(fs::remove_dir_all(&root)))]);
    Ok(())
}

/// Claim 2: a single component of 254..1000 bytes, as the last component and as a parent.
fn component(label: &str, dir: &Path) -> io::Result<()> {
    let base = fs::canonicalize(dir)?.join("C");
    fs::create_dir(&base)?;
    let claim = "2-component";
    for size in [254usize, 255, 256, 300, 1000] {
        let path = base.join("a".repeat(size));
        let len = path.as_os_str().len();
        probe(claim, &format!("{label}:last-component-{size}-absent"), len, &path, RmOrder::FileFirst, false);
        let created = fs::File::create(&path);
        row(claim, &format!("{label}:last-component-{size}-create"), len, &[("create", crate::calls::show(created))]);
        if path.symlink_metadata().is_ok() {
            probe(claim, &format!("{label}:last-component-{size}-exists"), len, &path, RmOrder::FileFirst, false);
        }

        let made = fs::create_dir(&path);
        row(claim, &format!("{label}:parent-component-{size}-mkdir"), len, &[("mkdir", crate::calls::show(made))]);
        let child = path.join("x");
        probe(claim, &format!("{label}:parent-component-{size}"), child.as_os_str().len(), &child, RmOrder::FileFirst, false);
        let _ = fs::remove_dir(&path);
    }
    row("cleanup", label, 0, &[("remove_dir_all", crate::calls::show(fs::remove_dir_all(&base)))]);
    Ok(())
}
