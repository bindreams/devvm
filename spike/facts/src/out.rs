//! Result lines. Every `RESULT` line is evidence.
use std::fmt::Display;
use std::io;
use std::sync::OnceLock;

static LABEL: OnceLock<String> = OnceLock::new();

pub fn set_label(label: &str) {
    LABEL.set(label.to_owned()).expect("label set once");
}

pub fn row(group: &str, text: impl Display) {
    println!("RESULT {group} fs={} {text}", LABEL.get().map_or("-", String::as_str));
}

/// `Err(Kind, os=N)`; on Unix the strerror text is added, since the errno name is what the table needs.
pub fn err(e: &io::Error) -> String {
    let os = e.raw_os_error().map_or_else(|| "-".to_owned(), |n| n.to_string());
    #[cfg(unix)]
    {
        let text = e.to_string();
        let text = text.split(" (os error").next().unwrap_or("");
        format!("Err({:?}, os={os}, \"{text}\")", e.kind())
    }
    #[cfg(not(unix))]
    format!("Err({:?}, os={os}{})", e.kind(), if e.raw_os_error().is_none() { format!(", \"{e}\"") } else { String::new() })
}

pub fn show<T>(r: &io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".to_owned(),
        Err(e) => err(e),
    }
}

pub fn show_with<T>(r: &io::Result<T>, f: impl FnOnce(&T) -> String) -> String {
    match r {
        Ok(v) => format!("ok({})", f(v)),
        Err(e) => err(e),
    }
}

pub fn kind_of(meta: &std::fs::Metadata) -> &'static str {
    let t = meta.file_type();
    if t.is_symlink() {
        "symlink"
    } else if t.is_dir() {
        "dir"
    } else if t.is_file() {
        "file"
    } else {
        "other"
    }
}
