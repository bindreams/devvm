//! Commands that need no VM and no QEMU, through the real binary.

use std::process::Output;

#[path = "support/home.rs"]
mod home;
use home::home;
#[path = "support/isolate.rs"]
mod isolate;
#[path = "support/unicode.rs"]
mod unicode;

const MANIFEST: &str = include_str!("../../devvm/qemu-manifest.toml");

fn main() {
    skuld::run_all();
}

fn stdout_lines(output: &Output) -> Vec<String> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[skuld::test]
fn doctor_prints_every_check_then_exits_125_iff_any_line_says_fail() {
    let home = home();
    let cwd = home.project("a");

    let output = home.devvm(&cwd, &["doctor"]);

    let lines = stdout_lines(&output);
    let failed = lines.iter().filter(|line| line.starts_with("fail ")).count();
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("pass ") || line.starts_with("fail ")),
        "{lines:?}"
    );
    let names = [
        "accelerator",
        "free space (data)",
        "free space (cache)",
        "free space (state)",
        "ssh",
        "git",
        "qemu",
    ];
    for name in names {
        assert!(
            lines.iter().any(|line| line[5..].starts_with(&format!("{name}:"))),
            "no line for {name}: {lines:?}"
        );
    }
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    if failed == 0 {
        assert_eq!(output.status.code(), Some(0), "{stderr:?}");
    } else {
        assert_eq!(output.status.code(), Some(125), "{stderr:?}");
        assert_eq!(
            stderr.lines().last(),
            Some(format!("devvm-error: unavailable: {failed} check(s) failed").as_str()),
            "{stderr:?}"
        );
    }
    // Nothing is downloaded in a fresh root, so a build in the manifest is a pass.
    let qemu = lines.iter().find(|line| line[5..].starts_with("qemu:")).unwrap();
    if MANIFEST.contains(&format!("[targets.{}]", devvm::TARGET)) {
        assert!(qemu.starts_with("pass "), "{qemu}");
    }
}
