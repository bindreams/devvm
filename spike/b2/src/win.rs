//! Claims 3, 4 and 5: long paths, a missing drive letter, and UNC paths whose server or share is missing.

use crate::calls::{RmOrder, probe, row, show};
use crate::ladder::{Kind, Ladder, absent_path, remove_all};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;

pub fn run(command: &str, label: &str, dir: &Path, max_length: Option<usize>) -> io::Result<()> {
    match command {
        "long" => long(label, dir, max_length),
        "drive" => drive(label, dir),
        "unc" => unc(label),
        "sweep" => remove_all(&dir.join("L")),
        other => Err(io::Error::other(format!("unknown command {other}"))),
    }
}

fn len(p: &Path) -> usize {
    // The paths are ASCII, so bytes are UTF-16 units.
    p.as_os_str().len()
}

fn verbatim(p: &Path) -> PathBuf {
    PathBuf::from(format!(r"\\?\{}", p.display()))
}

/// Puts a probed name back: the probe's `remove_*` calls take it away.
fn restore(kind: Kind, path: &Path) -> String {
    show(match kind {
        Kind::File => fs::File::create(path).map(drop),
        Kind::Dir => fs::create_dir(path),
    })
}

/// Claim 3: existing and absent names at exact full-path lengths, through std (plain form) and with `\\?\` spelled out.
fn long(label: &str, dir: &Path, max_length: Option<usize>) -> io::Result<()> {
    let base = std::path::absolute(dir)?;
    let root = base.join("L");
    let root_len = len(&root);
    let mut lengths: Vec<usize> = vec![250, 259, 260, 261, 262, 300, 1000, 5000, 20000, 32000];
    lengths.extend(32690..=32770);
    lengths.push(33000);
    lengths.retain(|&n| n >= root_len + 4 && max_length.is_none_or(|m| n <= m));
    row("env", label, 0, &[("root_len", root_len.to_string()), ("base", base.display().to_string())]);
    let jobs: Vec<(usize, Kind)> = lengths.iter().flat_map(|&n| [(n - 2, Kind::File), (n, Kind::Dir)]).collect();

    let mut ladder = Ladder::new(root.clone(), false, 640)?;
    let leaves = ladder.build(&jobs);

    let claim = "3-long";
    for n in lengths {
        let leaf = |target, kind| leaves.iter().find(|l| l.target == target && l.kind == kind).expect("leaf was planned");
        for (form, wrap) in [("plain", false), ("verbatim", true)] {
            let shape = |p: &Path| if wrap { verbatim(p) } else { p.to_path_buf() };
            let case = |what: &str| format!("{label}:{form}-{what}");
            match &leaf(n - 2, Kind::File).path {
                Ok(dir) => {
                    let file = dir.join("f");
                    let p = shape(&file);
                    probe(claim, &case("file-exists"), len(&file), &p, RmOrder::FileFirst, false);
                    let r = restore(Kind::File, &file);
                    row(claim, &case("file-restored"), len(&file), &[("create", r)]);
                    let absent = dir.join("g");
                    probe(claim, &case("file-absent"), len(&absent), &shape(&absent), RmOrder::FileFirst, false);
                }
                Err(why) => row(claim, &case("file-exists"), n, &[("build", why.clone())]),
            }
            match &leaf(n, Kind::Dir).path {
                Ok(dir) => {
                    probe(claim, &case("dir-exists"), len(dir), &shape(dir), RmOrder::DirFirst, false);
                    let r = restore(Kind::Dir, dir);
                    row(claim, &case("dir-restored"), len(dir), &[("mkdir", r)]);
                }
                Err(why) => row(claim, &case("dir-exists"), n, &[("build", why.clone())]),
            }
            let nothing = absent_path(&root, n);
            probe(claim, &case("nothing-exists"), len(&nothing), &shape(&nothing), RmOrder::FileFirst, false);
        }
    }
    row("cleanup", label, 0, &[("remove_dir_all", show(remove_all(&root)))]);
    Ok(())
}

/// Claim 4: a drive letter with no volume, against a letter that has one.
fn drive(label: &str, dir: &Path) -> io::Result<()> {
    let mask = unsafe { GetLogicalDrives() };
    let used = |c: char| mask & (1 << (c as u8 - b'A')) != 0;
    let in_use: String = ('A'..='Z').filter(|&c| used(c)).collect();
    let unused = ('Q'..='Z').rev().chain(('A'..='P').rev()).find(|&c| !used(c)).expect("a free drive letter");
    let present = std::path::absolute(dir)?.display().to_string().chars().next().expect("drive letter");
    row("env", label, 0, &[("in_use", in_use), ("missing_drive", unused.to_string()), ("present_drive", present.to_string())]);

    let claim = "4-drive";
    let missing = [
        (format!(r"{unused}:\f"), "root-file"),
        (format!(r"{unused}:\a\b"), "nested"),
        (format!(r"{unused}:\"), "root"),
        (format!(r"{unused}:f"), "drive-relative"),
        (format!(r"\\?\{unused}:\f"), "verbatim-root-file"),
        (format!(r"\\?\{unused}:\a\b"), "verbatim-nested"),
        (format!(r"\\?\{unused}:\"), "verbatim-root"),
    ];
    for (path, what) in missing {
        let p = PathBuf::from(path);
        probe(claim, &format!("{label}:missing-drive-{what}"), len(&p), &p, RmOrder::FileFirst, false);
    }
    let control = [
        (format!(r"{present}:\b2-no-such-file"), "present-drive-missing-file"),
        (format!(r"{present}:\b2-no-such-dir\f"), "present-drive-missing-dir"),
        (format!(r"\\?\{present}:\b2-no-such-dir\f"), "present-drive-missing-dir-verbatim"),
    ];
    for (path, what) in control {
        let p = PathBuf::from(path);
        probe(claim, &format!("{label}:{what}"), len(&p), &p, RmOrder::FileFirst, false);
    }
    Ok(())
}

/// Claim 5: UNC paths, timed. Only with `SPIKE_NET=1`: the calls leave the machine.
fn unc(label: &str) -> io::Result<()> {
    if std::env::var_os("SPIKE_NET").is_none_or(|v| v != "1") {
        return Err(io::Error::other("SPIKE_NET=1 is not set; UNC paths are measured in CI only"));
    }
    let claim = "5-unc";
    let paths = [
        (r"\\nonexistent-host-xyz\share\f", "unreachable-host-file"),
        (r"\\nonexistent-host-xyz\share", "unreachable-host-share"),
        (r"\\?\UNC\nonexistent-host-xyz\share\f", "unreachable-host-file-verbatim"),
        (r"\\localhost\no-such-share\f", "missing-share-file"),
        (r"\\localhost\no-such-share", "missing-share"),
        (r"\\?\UNC\localhost\no-such-share\f", "missing-share-file-verbatim"),
        (r"\\127.0.0.1\no-such-share\f", "missing-share-ip"),
        (r"\\localhost\C$\b2-no-such-dir\f", "admin-share-missing-dir"),
        (r"\\localhost\C$\b2-no-such-file", "admin-share-missing-file"),
    ];
    for (path, what) in paths {
        let p = PathBuf::from(path);
        probe(claim, &format!("{label}:{what}"), len(&p), &p, RmOrder::FileFirst, true);
    }
    Ok(())
}
