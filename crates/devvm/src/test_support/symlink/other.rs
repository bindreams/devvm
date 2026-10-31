use std::path::Path;

/// A directory symlink if `target` is a directory, else a file symlink (also for a target that does not exist). A
/// relative `target` is read from the directory of `link`, as the file system reads it.
pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    let resolved = link
        .parent()
        .map_or_else(|| target.to_owned(), |parent| parent.join(target));
    if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
