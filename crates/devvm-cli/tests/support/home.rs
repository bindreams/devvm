//! A temp directory holding the Unicode test directory, where a test's five XDG directories and its project directories
//! live, and the `devvm` binary run against them. The including test declares the `isolate` and `unicode` modules.

#![allow(dead_code, reason = "each test target that includes this file uses part of it")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::isolate::isolate;
use super::unicode::UNICODE_DIR;

pub struct Home {
    _tmp: Option<skuld::TempDir>,
    pub root: PathBuf,
    /// `XDG_CACHE_HOME`, where it is not `<root>/cache`.
    cache: Option<PathBuf>,
    /// `XDG_RUNTIME_DIR`, where it is not `<root>/run`.
    run: Option<PathBuf>,
}

pub fn home() -> Home {
    let tmp = skuld::TempDir::new().unwrap();
    let root = tmp.join(UNICODE_DIR);
    std::fs::create_dir(&root).unwrap();
    Home {
        _tmp: Some(tmp),
        root,
        cache: None,
        run: None,
    }
}

impl Home {
    /// The directories of a test that made `root` itself and keeps it alive: its cache is `cache`, which other
    /// processes share, and its runtime directory is `run`, which exists and is short enough for a control socket.
    pub fn at(root: &Path, cache: &Path, run: &Path) -> Home {
        Home {
            _tmp: None,
            root: root.to_path_buf(),
            cache: Some(cache.to_path_buf()),
            run: Some(run.to_path_buf()),
        }
    }

    pub fn state(&self) -> PathBuf {
        self.root.join("state").join("devvm")
    }

    /// devvm's runtime directory, which `reap` sweeps.
    pub fn run(&self) -> PathBuf {
        self.run.clone().unwrap_or_else(|| self.root.join("run")).join("devvm")
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config").join("devvm").join("config.yaml")
    }

    pub fn project(&self, name: &str) -> PathBuf {
        let path = self.root.join("projects").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The command `devvm <args>` in `cwd`, with no stdin.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devvm"));
        command.current_dir(cwd).args(args).env_remove("DEVVM_STATUS_FILE");
        isolate(&mut command, &self.root);
        if let Some(cache) = &self.cache {
            command.env("XDG_CACHE_HOME", cache);
        }
        if let Some(run) = &self.run {
            command.env("XDG_RUNTIME_DIR", run);
        }
        command.stdin(Stdio::null());
        command
    }

    pub fn devvm(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }
}

impl Home {
    /// Writes the files of a stopped instance the way a devvm with schema 1 does.
    pub fn seed(&self, name: &str, id: &str, created_from: &Path, owner: &str) {
        let dir = self.state().join("instances").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let from = serde_json::to_string(created_from.to_str().unwrap()).unwrap();
        let record = RECORD
            .replace("@NAME@", name)
            .replace("@ID@", id)
            .replace("@FROM@", &from);
        std::fs::write(dir.join("record.json"), record).unwrap();
        let marker = format!(r#"{{"schema":1,"owner":"{owner}","id":"{id}"}}"#);
        std::fs::write(dir.join("owner.json"), marker).unwrap();
        std::fs::write(dir.join("disk.img"), "disk").unwrap();
        // Every instance has its two lock files, created with it.
        let locks = self.state().join("locks");
        std::fs::create_dir_all(&locks).unwrap();
        for kind in ["owner", "op"] {
            std::fs::write(locks.join(format!("instance-{id}.{kind}")), "").unwrap();
        }
    }
}

/// `record.json` of a stopped instance, as schema 1 defines it.
pub const RECORD: &str = r#"{
  "schema": 1,
  "id": "@ID@",
  "name": "@NAME@",
  "created": "2023-11-14T22:13:20Z",
  "created_from": @FROM@,
  "guest": "linux",
  "arch": "amd64",
  "chain": [
    {
      "key": "01010101010101010101010101010101",
      "generation": "02020202020202020202020202020202"
    }
  ],
  "expires": null,
  "cpus": 2,
  "memory": 4294967296,
  "network": "nat",
  "machine": null
}"#;

pub fn id(n: u8) -> String {
    format!("{n:02x}").repeat(16)
}

pub fn stdout_lines(output: &Output) -> Vec<String> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

pub fn one_error_line(output: &Output, kind: &str) -> String {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr:?}");
    assert!(stderr.starts_with(&format!("devvm-error: {kind}: ")), "{stderr:?}");
    stderr
}
