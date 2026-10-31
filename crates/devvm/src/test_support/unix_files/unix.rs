use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;

/// Makes a FIFO at `path`.
pub fn make_fifo(path: &Path) {
    let made = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("the test needs the `mkfifo` command");
    assert!(made.success(), "mkfifo {} failed", path.display());
}

/// A file in `dir` whose name is exactly `name`, which need not be valid UTF-8.
pub fn make_file_with_name_bytes(dir: &Path, name: &[u8]) {
    std::fs::write(dir.join(OsString::from_vec(name.to_vec())), b"x").expect("the file can be made");
}

/// A symlink at `link` that holds exactly `target`, which need not be valid UTF-8.
pub fn make_symlink_with_target_bytes(target: &[u8], link: &Path) {
    std::os::unix::fs::symlink(OsString::from_vec(target.to_vec()), link).expect("the symlink can be made");
}
