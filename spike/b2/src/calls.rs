//! The seven std calls, run on one path, printed as one line.

use std::fs;
use std::io;
use std::path::Path;
use std::time::Instant;

/// Which of `remove_file` and `remove_dir` runs first: the second finds the name gone when the first succeeds.
#[derive(Clone, Copy)]
pub enum RmOrder {
    FileFirst,
    DirFirst,
}

/// `Kind,code` with `-` for an error without an OS code.
pub fn err(e: &io::Error) -> String {
    match e.raw_os_error() {
        Some(code) => format!("{:?},{code}", e.kind()),
        None => format!("{:?},-", e.kind()),
    }
}

pub fn show<T>(r: io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => err(&e),
    }
}

/// Prints `RESULT<TAB>claim<TAB>case<TAB>len<TAB>name=value...`.
pub fn row(claim: &str, case: &str, len: usize, fields: &[(&str, String)]) {
    let fields: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!("RESULT\t{claim}\t{case}\t{len}\t{}", fields.join("\t"));
}

/// Runs the seven calls on `path`. With `timed`, each result carries its duration in milliseconds.
pub fn probe(claim: &str, case: &str, len: usize, path: &Path, order: RmOrder, timed: bool) {
    let mut fields: Vec<(&str, String)> = Vec::new();
    let mut run = |name: &'static str, f: &dyn Fn() -> String| {
        let start = Instant::now();
        let mut out = f();
        if timed {
            out = format!("{out} [{}ms]", start.elapsed().as_millis());
        }
        fields.push((name, out));
    };
    run("symlink_metadata", &|| show(fs::symlink_metadata(path)));
    run("metadata", &|| show(fs::metadata(path)));
    run("open", &|| show(fs::File::open(path)));
    run("read_dir", &|| show(fs::read_dir(path)));
    run("try_exists", &|| match path.try_exists() {
        Ok(b) => format!("ok({b})"),
        Err(e) => err(&e),
    });
    match order {
        RmOrder::FileFirst => {
            run("remove_dir", &|| show(fs::remove_dir(path)));
            run("remove_file", &|| show(fs::remove_file(path)));
        }
        RmOrder::DirFirst => {
            run("remove_file", &|| show(fs::remove_file(path)));
            run("remove_dir", &|| show(fs::remove_dir(path)));
        }
    }
    row(claim, case, len, &fields);
}
