//! What a failed command leaves on stderr.

use std::process::Output;

/// The `devvm-error:` line of a failed command, which is stderr's last and its only one. A command that got as far
/// as running names its log on the line before it; any lines before that are its progress.
pub fn one_error_line(output: &Output, kind: &str) -> String {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let lines: Vec<&str> = stderr.lines().collect();
    let (line, before) = lines
        .split_last()
        .unwrap_or_else(|| panic!("no error line: {stderr:?}"));
    assert!(line.starts_with(&format!("devvm-error: {kind}: ")), "{stderr:?}");
    if let Some(log) = before.last() {
        assert!(log.starts_with("log: "), "{stderr:?}");
    }
    assert!(
        !before.iter().any(|line| line.starts_with("devvm-error:")),
        "{stderr:?}"
    );
    format!("{line}\n")
}
