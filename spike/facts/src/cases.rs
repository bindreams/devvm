//! The states a path can be in: absent in several ways, refused in several ways, present, delete-pending.
use std::any::Any;
use std::io;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

use crate::out::show;
use crate::util::{Share, open_read, touch};

/// What a case set up. Dropping it releases the open handles (`keep`).
pub struct Made {
    pub path: PathBuf,
    pub keep: Vec<Box<dyn Any>>,
    pub note: String,
    pub restore: Vec<Restore>,
    /// The directory the case was made in; set by the runner.
    pub scratch: PathBuf,
}

pub enum Restore {
    #[cfg(unix)]
    Chmod(PathBuf),
    #[cfg(windows)]
    Icacls(PathBuf),
}

impl Made {
    pub fn at(path: PathBuf) -> io::Result<Made> {
        Ok(Made {
            path,
            keep: Vec::new(),
            note: String::new(),
            restore: Vec::new(),
            scratch: PathBuf::new(),
        })
    }
}

pub struct Case {
    pub name: String,
    pub make: Box<dyn Fn(&Path) -> io::Result<Made>>,
}

pub fn case(name: &str, make: impl Fn(&Path) -> io::Result<Made> + 'static) -> Case {
    Case {
        name: name.to_owned(),
        make: Box::new(make),
    }
}

pub fn skipped(why: &str) -> io::Error {
    io::Error::other(format!("skipped: {why}"))
}

pub fn symlink(target: &Path, link: &Path, dir: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        let _ = dir;
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        if dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }
}

pub fn all() -> Vec<Case> {
    let sep = MAIN_SEPARATOR;
    let mut v = vec![
        case("missing", |s| Made::at(s.join("missing"))),
        case("missing-parent", |s| Made::at(s.join("nodir").join("x"))),
        case("parent-is-file", |s| {
            touch(&s.join("f"))?;
            Made::at(s.join("f").join("x"))
        }),
        case("trailing-separator-on-file", move |s| {
            touch(&s.join("f"))?;
            Made::at(PathBuf::from(format!("{}{sep}f{sep}", s.display())))
        }),
        case("trailing-separator-on-missing", move |s| {
            Made::at(PathBuf::from(format!("{}{sep}missing{sep}", s.display())))
        }),
        case("present-file", |s| {
            touch(&s.join("f"))?;
            Made::at(s.join("f"))
        }),
        case("present-dir", |s| {
            std::fs::create_dir(s.join("d"))?;
            Made::at(s.join("d"))
        }),
        case("empty-path", |_| Made::at(PathBuf::new())),
        case("nul-in-name", move |s| {
            Made::at(PathBuf::from(format!("{}{sep}a\0b", s.display())))
        }),
        case("component-300-chars", |s| Made::at(s.join("a".repeat(300)))),
        case("component-256-chars", |s| Made::at(s.join("a".repeat(256)))),
        case("path-over-300-bytes-total", |s| {
            let mut p = s.to_owned();
            for _ in 0..4 {
                p.push("d".repeat(100));
            }
            p.push("x");
            Made::at(p)
        }),
    ];
    for (label, name) in [
        ("pipe", "a|b"),
        ("star", "a*b"),
        ("question", "a?b"),
        ("quote", "a\"b"),
        ("less", "a<b"),
        ("greater", "a>b"),
        ("colon", "a:b"),
        ("trailing-space", "a "),
        ("leading-space", " a"),
        ("trailing-dot", "a."),
        ("only-dots", "..."),
        ("backslash-inside", "a\\b"),
    ] {
        v.push(case(&format!("name-{label}"), move |s| Made::at(s.join(name))));
    }
    // Device names are opened as devices on Windows, so they run in CI only.
    for name in ["NUL", "CON", "COM1", "con.txt", "aux", "LPT1"] {
        v.push(case(&format!("name-reserved-{name}"), move |s| {
            if cfg!(windows) && std::env::var_os("SPIKE_RESERVED").is_none() {
                return Err(skipped("SPIKE_RESERVED unset"));
            }
            Made::at(s.join(name))
        }));
    }
    v.extend([
        case("symlink-dangling", |s| {
            symlink(&s.join("nowhere"), &s.join("l"), false)?;
            Made::at(s.join("l"))
        }),
        case("symlink-loop", |s| {
            symlink(&s.join("b"), &s.join("a"), false)?;
            symlink(&s.join("a"), &s.join("b"), false)?;
            Made::at(s.join("a"))
        }),
        case("symlink-to-file", |s| {
            touch(&s.join("f"))?;
            symlink(&s.join("f"), &s.join("l"), false)?;
            Made::at(s.join("l"))
        }),
        case("symlink-to-dir", |s| {
            std::fs::create_dir(s.join("d"))?;
            symlink(&s.join("d"), &s.join("l"), true)?;
            Made::at(s.join("l"))
        }),
        case("symlink-dangling-into-missing-dir", |s| {
            symlink(&s.join("nodir").join("x"), &s.join("l"), false)?;
            Made::at(s.join("l"))
        }),
        case("symlink-dangling-into-existing-dir", |s| {
            std::fs::create_dir(s.join("d"))?;
            symlink(&s.join("d").join("x"), &s.join("l"), false)?;
            Made::at(s.join("l"))
        }),
        case("readonly-file", |s| {
            let p = s.join("f");
            touch(&p)?;
            let mut perms = std::fs::metadata(&p)?.permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&p, perms)?;
            Made::at(p)
        }),
        case("file-denied", denied_file),
        case("dir-denied-child-inside", denied_dir),
        case("unlinked-while-open", |s| {
            let p = s.join("f");
            touch(&p)?;
            let keeper = open_read(&p, Share::All)?;
            let removed = std::fs::remove_file(&p);
            let mut m = Made::at(p)?;
            m.note = format!("remove_file with a handle open: {}", show(&removed));
            m.keep.push(Box::new(keeper));
            Ok(m)
        }),
        case("tree-removed-while-file-open", |s| {
            let d = s.join("d");
            std::fs::create_dir(&d)?;
            touch(&d.join("f"))?;
            let keeper = open_read(&d.join("f"), Share::All)?;
            let removed = std::fs::remove_dir_all(&d);
            let mut m = Made::at(d)?;
            m.note = format!("remove_dir_all with a child open: {}", show(&removed));
            m.keep.push(Box::new(keeper));
            Ok(m)
        }),
        case("tree-removed-while-file-open-no-share-delete", |s| {
            let d = s.join("d");
            std::fs::create_dir(&d)?;
            touch(&d.join("f"))?;
            let keeper = open_read(&d.join("f"), Share::NoDelete)?;
            let removed = std::fs::remove_dir_all(&d);
            let mut m = Made::at(d)?;
            m.note = format!("remove_dir_all with a child open without delete sharing: {}", show(&removed));
            m.keep.push(Box::new(keeper));
            Ok(m)
        }),
    ]);
    #[cfg(windows)]
    v.extend(crate::win::pending_cases());
    v
}

#[cfg(unix)]
fn denied_file(s: &Path) -> io::Result<Made> {
    use std::os::unix::fs::PermissionsExt;
    let p = s.join("f");
    touch(&p)?;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0))?;
    let mut m = Made::at(p.clone())?;
    m.restore.push(Restore::Chmod(p));
    Ok(m)
}

#[cfg(unix)]
fn denied_dir(s: &Path) -> io::Result<Made> {
    use std::os::unix::fs::PermissionsExt;
    let d = s.join("d");
    std::fs::create_dir(&d)?;
    touch(&d.join("f"))?;
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0))?;
    let mut m = Made::at(d.join("f"))?;
    m.restore.push(Restore::Chmod(d));
    Ok(m)
}

#[cfg(windows)]
fn denied_file(s: &Path) -> io::Result<Made> {
    if std::env::var_os("SPIKE_ACLS").is_none() {
        return Err(skipped("SPIKE_ACLS unset"));
    }
    let p = s.join("f");
    touch(&p)?;
    crate::win::deny_everyone(&p)?;
    let mut m = Made::at(p.clone())?;
    m.restore.push(Restore::Icacls(p));
    Ok(m)
}

#[cfg(windows)]
fn denied_dir(s: &Path) -> io::Result<Made> {
    if std::env::var_os("SPIKE_ACLS").is_none() {
        return Err(skipped("SPIKE_ACLS unset"));
    }
    let d = s.join("d");
    std::fs::create_dir(&d)?;
    touch(&d.join("f"))?;
    crate::win::deny_everyone(&d)?;
    let mut m = Made::at(d.join("f"))?;
    m.restore.push(Restore::Icacls(d));
    Ok(m)
}

impl Restore {
    pub fn apply(&self) {
        match self {
            #[cfg(unix)]
            Restore::Chmod(p) => {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
            }
            #[cfg(windows)]
            Restore::Icacls(p) => crate::win::allow_everyone(p),
        }
    }
}
