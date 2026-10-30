use std::path::Path;

/// A directory symlink if `target` is a directory, else a file symlink (also for a target that does not exist).
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
