use std::path::Path;

pub fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// No error of a Unix host says that the user may not create symlinks.
pub fn lacks_the_right(_error: &std::io::Error) -> bool {
    false
}
