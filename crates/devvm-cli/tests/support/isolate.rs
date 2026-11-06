//! Points a command at a test's own five directories, so it neither reads the user's `config.yaml` nor sweeps the
//! user's runtime directory.

use std::path::Path;
use std::process::Command;

/// Points the five XDG variables at subdirectories of `root`.
pub fn isolate(command: &mut Command, root: &Path) {
    for (name, dir) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        command.env(name, root.join(dir));
    }
}
