//! Points a command at a test's own five directories, so it neither reads the user's `config.yaml` nor sweeps the
//! user's runtime directory.

use std::path::Path;
use std::process::Command;

/// Points the five XDG variables at subdirectories of `root`.
pub fn isolate(command: &mut Command, root: &Path) {
    for (name, dir) in devvm::Dirs::env_under(root) {
        command.env(name, dir);
    }
}
