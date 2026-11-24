//! Reading and setting Unix file modes from tests that run on every OS: off Unix there are no modes.

#[cfg(unix)]
#[path = "file_mode/unix.rs"]
mod imp;
#[cfg(not(unix))]
#[path = "file_mode/other.rs"]
mod imp;

pub use imp::{mode, set_mode};

/// Replaces the file `path` by a copy that others can read: mode `0644` where there are modes, and what the directory
/// hands down on Windows. What a file made before devvm created its private files private looks like.
pub fn loosen(path: &std::path::Path) {
    let text = std::fs::read(path).unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::write(path, text).unwrap();
    set_mode(path, 0o644);
}
