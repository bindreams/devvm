//! Claim 3c: the Windows path limit on an existing directory (and file) through different spellings of the same volume.
//!
//! `forms <C|D|UNC> <base>` builds under `<base>` as given (`C:\x`, `D:\x`, `\\localhost\C$\x`); `forms subst <dir>`
//! builds a deep directory under `<dir>`, maps a free drive letter to it, and builds under the drive root.

use super::{len, restore, verbatim};
use crate::calls::{RmOrder, err, probe, row, show};
use crate::ladder::{Kind, Ladder, STRIDE, remove_all};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    DDD_NO_BROADCAST_SYSTEM, DDD_RAW_TARGET_PATH, DDD_REMOVE_DEFINITION, DDD_EXACT_MATCH_ON_REMOVE, DefineDosDeviceW,
};

const CLAIM: &str = "3c-edge";
/// Real full-path lengths probed, window plus samples below it.
const WINDOW: (usize, usize) = (32400, 32770);
const SAMPLES: [usize; 5] = [260, 1000, 5000, 20000, 32000];

pub fn run(form: &str, dir: &Path) -> io::Result<()> {
    match form {
        "subst" => subst(dir),
        _ => {
            fs::create_dir_all(dir)?;
            let base = std::path::absolute(dir)?;
            edge_form(form, &base, WINDOW, &SAMPLES)
        }
    }
}

/// Existing directories and files at every length in `window` and at the `samples`, then a rename extension.
fn edge_form(tag: &str, base: &Path, window: (usize, usize), samples: &[usize]) -> io::Result<()> {
    let root = base.join("E");
    let root_len = len(&root);
    let (lo, hi) = window;
    // The deepest chain node leaves a leaf name of at most 255 characters at the longest length.
    let cap = (hi.saturating_sub(256 + root_len)).div_ceil(STRIDE);
    row("env", tag, 0, &[("root", root.display().to_string()), ("root_len", root_len.to_string()), ("cap", cap.to_string())]);

    let mut jobs: Vec<(usize, Kind)> = (lo..=hi).map(|t| (t, Kind::Dir)).collect();
    for &t in samples {
        jobs.push((t, Kind::Dir));
        jobs.push((t - 2, Kind::File));
    }
    jobs.retain(|&(t, _)| t >= root_len + 2);
    let leaves = Ladder::new(root.clone(), false, cap)?.build(&jobs);

    let mut longest: Option<PathBuf> = None;
    for leaf in &leaves {
        let what = if leaf.kind == Kind::Dir { "dir" } else { "file" };
        let case = |form: &str| format!("{tag}:{what}-{form}");
        match &leaf.path {
            Ok(d) if leaf.kind == Kind::Dir => {
                for (form, p) in [("plain", d.clone()), ("verbatim", verbatim(d))] {
                    probe(CLAIM, &case(form), len(d), &p, RmOrder::DirFirst, false);
                    let r = restore(Kind::Dir, d);
                    row(CLAIM, &case(&format!("{form}-restored")), len(d), &[("mkdir", r)]);
                }
                if longest.as_ref().is_none_or(|l| len(l) < len(d)) {
                    longest = Some(d.clone());
                }
            }
            Ok(d) => {
                let file = d.join("f");
                for (form, p) in [("plain", file.clone()), ("verbatim", verbatim(&file))] {
                    probe(CLAIM, &case(form), len(&file), &p, RmOrder::FileFirst, false);
                    let r = restore(Kind::File, &file);
                    row(CLAIM, &case(&format!("{form}-restored")), len(&file), &[("create", r)]);
                }
            }
            Err(why) => row(CLAIM, &case("build"), leaf.target + if leaf.kind == Kind::File { 2 } else { 0 }, &[("build", why.clone())]),
        }
    }

    if let Some(mut cur) = longest {
        let parent = cur.parent().expect("leaf has a parent").to_path_buf();
        loop {
            let name_len = cur.file_name().expect("leaf has a name").len() + 1;
            let next = parent.join("R".repeat(name_len));
            let case = |form: &str| format!("{tag}:renamed-dir-{form}");
            if let Err(e) = fs::rename(&cur, &next) {
                row(CLAIM, &case("plain"), len(&next), &[("rename", err(&e))]);
                break;
            }
            for (form, p) in [("plain", next.clone()), ("verbatim", verbatim(&next))] {
                probe(CLAIM, &case(form), len(&next), &p, RmOrder::DirFirst, false);
                let r = restore(Kind::Dir, &next);
                row(CLAIM, &case(&format!("{form}-restored")), len(&next), &[("mkdir", r)]);
            }
            cur = next;
        }
    }
    row("cleanup", tag, 0, &[("remove_all", show(remove_all(&root)))]);
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// A drive letter mapped to a directory; the mapping is removed on drop.
struct Mapping {
    device: Vec<u16>,
    target: Vec<u16>,
}

impl Mapping {
    fn new(letter: char, dir: &Path) -> io::Result<Mapping> {
        let device = wide(&format!("{letter}:"));
        let target = wide(&format!(r"\??\{}", dir.display()));
        let ok = unsafe { DefineDosDeviceW(DDD_RAW_TARGET_PATH | DDD_NO_BROADCAST_SYSTEM, device.as_ptr(), target.as_ptr()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { device, target })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let flags = DDD_RAW_TARGET_PATH | DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE | DDD_NO_BROADCAST_SYSTEM;
        let ok = unsafe { DefineDosDeviceW(flags, self.device.as_ptr(), self.target.as_ptr()) };
        let r = if ok == 0 { err(&io::Error::last_os_error()) } else { "ok".into() };
        row("unmap", "subst", 0, &[("DefineDosDevice remove", r)]);
    }
}

/// A drive letter mapped to a directory whose own path is as long as the mapping accepts: the path std sees is
/// short, the path the volume resolves is long.
fn subst(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let scratch = std::path::absolute(dir)?.join("S");
    let candidates = [16001, 8001, 4001, 2001, 1001, 501, 301, 261, 250];
    let jobs: Vec<(usize, Kind)> = candidates.iter().map(|&t| (t, Kind::Dir)).collect();
    let leaves = Ladder::new(scratch.clone(), false, 400)?.build(&jobs);
    let (_, letter) = super::drive_letters();
    let mut result = Ok(());
    // Longest candidate first: `leaves` is sorted ascending.
    for leaf in leaves.iter().rev() {
        let Ok(deep) = &leaf.path else { continue };
        let d0 = len(deep);
        match Mapping::new(letter, deep) {
            Ok(_mapping) => {
                row("env", "subst", 0, &[("letter", letter.to_string()), ("target", deep.display().to_string()), ("target_len", d0.to_string())]);
                // `X:\E\...` resolves to `<deep>\E\...`: real length = std length - 2 + d0.
                let window = (WINDOW.0 + 2 - d0, WINDOW.1 + 2 - d0);
                let samples = [260, 1000, 5000, 10000, WINDOW.0 - d0];
                result = edge_form("subst", Path::new(&format!(r"{letter}:\")), window, &samples);
                break;
            }
            Err(e) => row("map", "subst", d0, &[("DefineDosDevice", err(&e))]),
        }
    }
    row("cleanup", "subst-deep", 0, &[("remove_all", show(remove_all(&scratch)))]);
    result
}
