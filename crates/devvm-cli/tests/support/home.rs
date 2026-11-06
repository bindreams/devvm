//! A temp directory holding the Unicode test directory, where a test's five XDG directories and its project directories
//! live, and the `devvm` binary run against them. The including test declares the `isolate` and `unicode` modules.

#![allow(dead_code, reason = "each test target that includes this file uses part of it")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::isolate::isolate;
use super::unicode::UNICODE_DIR;

pub struct Home {
    _tmp: skuld::TempDir,
    pub root: PathBuf,
}

pub fn home() -> Home {
    let tmp = skuld::TempDir::new().unwrap();
    let root = tmp.join(UNICODE_DIR);
    std::fs::create_dir(&root).unwrap();
    Home { _tmp: tmp, root }
}

impl Home {
    pub fn state(&self) -> PathBuf {
        self.root.join("state").join("devvm")
    }

    /// devvm's runtime directory, which `reap` sweeps.
    pub fn run(&self) -> PathBuf {
        self.root.join("run").join("devvm")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config").join("devvm").join("config.yaml")
    }

    pub fn project(&self, name: &str) -> PathBuf {
        let path = self.root.join("projects").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    pub fn devvm(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devvm"));
        command.current_dir(cwd).args(args).env_remove("DEVVM_STATUS_FILE");
        isolate(&mut command, &self.root);
        command.stdin(Stdio::null()).output().unwrap()
    }
}
