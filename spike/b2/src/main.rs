//! Prints one `RESULT` line per probed path: the outcome of seven std file-system calls on it.
//!
//! Usage: `b2-spike <command> <label> <dir> [max-length]`. Commands: `special`, plus per platform
//! `long` and `component` (unix) or `long`, `drive` and `unc` (windows).

mod calls;
mod ladder;
mod special;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;

#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as platform;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [command, label, dir, rest @ ..] = args.as_slice() else {
        eprintln!("usage: b2-spike <command> <label> <dir> [max-length]");
        return ExitCode::from(2);
    };
    let max_length = rest.first().map(|s| s.parse::<usize>().expect("max-length is a number"));
    let dir = PathBuf::from(dir);
    let result = match command.as_str() {
        "special" => special::run(label, &dir),
        other => platform::run(other, label, &dir, max_length),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            println!("RESULT\terror\t{command}\t{label}\t{e}");
            ExitCode::FAILURE
        }
    }
}
