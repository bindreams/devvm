//! The calls devvm makes on a path, each reduced to its outcome.
use std::fs::{self, OpenOptions};

use crate::cases::Made;
use crate::out::{kind_of, show, show_with};
use crate::util::touch;

pub struct Call {
    pub name: &'static str,
    pub run: fn(&mut Made) -> String,
}

fn call(name: &'static str, run: fn(&mut Made) -> String) -> Call {
    Call { name, run }
}

pub fn all() -> Vec<Call> {
    vec![
        call("symlink_metadata", |m| {
            show_with(&fs::symlink_metadata(&m.path), |meta| kind_of(meta).to_owned())
        }),
        call("metadata", |m| {
            show_with(&fs::metadata(&m.path), |meta| kind_of(meta).to_owned())
        }),
        call("exists", |m| m.path.exists().to_string()),
        call("try_exists", |m| show_with(&m.path.try_exists(), |b| b.to_string())),
        call("is_dir", |m| m.path.is_dir().to_string()),
        call("is_file", |m| m.path.is_file().to_string()),
        call("open_read", |m| show(&fs::File::open(&m.path))),
        call("open_rw_existing", |m| {
            show(&OpenOptions::new().read(true).write(true).open(&m.path))
        }),
        call("open_rw_create", |m| {
            show(
                &OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&m.path),
            )
        }),
        call("create_new", |m| {
            show(&OpenOptions::new().write(true).create_new(true).open(&m.path))
        }),
        call("read_dir", |m| match fs::read_dir(&m.path) {
            Ok(entries) => format!("ok({} entries)", entries.count()),
            Err(e) => crate::out::err(&e),
        }),
        call("parent_listing", |m| {
            let Some(parent) = m.path.parent() else {
                return "no parent".to_owned();
            };
            let Some(name) = m.path.file_name() else {
                return "no file name".to_owned();
            };
            match fs::read_dir(parent) {
                Err(e) => crate::out::err(&e),
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Err(e) => return format!("entry {}", crate::out::err(&e)),
                            Ok(entry) if entry.file_name() == name => {
                                return format!("listed file_type={}", show_with(&entry.file_type(), |t| {
                                    if t.is_dir() {
                                        "dir"
                                    } else if t.is_file() {
                                        "file"
                                    } else if t.is_symlink() {
                                        "symlink"
                                    } else {
                                        "other"
                                    }
                                    .to_owned()
                                }));
                            }
                            Ok(_) => {}
                        }
                    }
                    "not listed".to_owned()
                }
            }
        }),
        call("remove_file", |m| show(&fs::remove_file(&m.path))),
        call("remove_dir", |m| show(&fs::remove_dir(&m.path))),
        call("rename_away", |m| {
            let to = m.path.with_file_name("moved-away");
            show(&fs::rename(&m.path, to))
        }),
        call("rename_over_it", |m| {
            let other = m.scratch.join("other");
            if let Err(e) = touch(&other) {
                return format!("setup {}", crate::out::err(&e));
            }
            show(&fs::rename(&other, &m.path))
        }),
        call("after_release", |m| {
            m.keep.clear();
            let meta = fs::symlink_metadata(&m.path);
            show_with(&meta, |meta| kind_of(meta).to_owned())
        }),
    ]
}
