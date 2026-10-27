use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// The permission bits of `path`.
pub fn mode(path: &Path) -> Option<u32> {
    let meta = std::fs::metadata(path).expect("the file should exist");
    Some(meta.permissions().mode() & 0o7777)
}

pub fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("the mode should be settable");
}
