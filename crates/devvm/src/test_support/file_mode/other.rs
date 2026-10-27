use std::path::Path;

/// There are no mode bits here.
pub fn mode(_path: &Path) -> Option<u32> {
    None
}

pub fn set_mode(_path: &Path, _mode: u32) {}
