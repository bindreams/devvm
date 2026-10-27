//! `ls`, `rm` and the commands that need a VM backend, through the real binary. The instances are seeded by writing
//! their files by hand, so the on-disk schema is pinned from outside the library.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn main() {
    skuld::run_all();
}

// Helpers =============================================================================================================

/// A temp dir holding `Anna Ж`, where the four XDG directories and the project directories live.
struct Home {
    _tmp: skuld::TempDir,
    root: PathBuf,
}

fn home() -> Home {
    let tmp = skuld::TempDir::new().unwrap();
    let root = tmp.join("Anna Ж");
    std::fs::create_dir(&root).unwrap();
    Home { _tmp: tmp, root }
}

impl Home {
    fn state(&self) -> PathBuf {
        self.root.join("state").join("devvm")
    }

    fn project(&self, name: &str) -> PathBuf {
        let path = self.root.join("projects").join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn devvm(&self, cwd: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devvm"));
        command.current_dir(cwd).args(args).env_remove("DEVVM_STATUS_FILE");
        for (name, dir) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_STATE_HOME", "state"),
        ] {
            command.env(name, self.root.join(dir));
        }
        command.stdin(Stdio::null()).output().unwrap()
    }

    /// Writes the files of a stopped instance the way a devvm with schema 1 does.
    fn seed(&self, name: &str, id: &str, created_from: &Path, owner: &str) {
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
const RECORD: &str = r#"{
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

fn id(n: u8) -> String {
    format!("{n:02x}").repeat(16)
}

fn stdout_lines(output: &Output) -> Vec<String> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn one_error_line(output: &Output, kind: &str) -> String {
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr:?}");
    assert!(stderr.starts_with(&format!("devvm-error: {kind}: ")), "{stderr:?}");
    stderr
}

// Tests ===============================================================================================================

#[skuld::test]
fn ls_json_emits_one_object_per_instance_and_filters_by_creation_dir() {
    let home = home();
    let (a, b) = (home.project("proj a"), home.project("Ж proj b"));
    home.seed("a1", &id(1), &a, "none");
    home.seed("a2", &id(2), &a, "process");
    // A live owner: this process holds the owner lock, so the reaper leaves a2 alone.
    let owner = std::fs::OpenOptions::new()
        .write(true)
        .open(home.state().join("locks").join(format!("instance-{}.owner", id(2))))
        .unwrap();
    owner.lock().unwrap();
    home.seed("b1", &id(3), &b, "none");

    let output = home.devvm(&a, &["ls", "--json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let lines = stdout_lines(&output);
    assert_eq!(lines.len(), 2, "{lines:?}");
    let objects: Vec<Value> = lines.iter().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(objects[0]["name"], "a1");
    assert_eq!(objects[1]["name"], "a2");
    for object in &objects {
        assert_eq!(object["state"], "stopped");
        assert_eq!(object["guest"], "linux");
        assert_eq!(object["arch"], "amd64");
        assert_eq!(object["created"], "2023-11-14T22:13:20Z");
        assert_eq!(object["created_from"], a.to_str().unwrap());
        assert_eq!(object["expires"], Value::Null);
    }
    assert_eq!(objects.len(), 2);
    for line in &lines {
        let keys = [
            "name",
            "state",
            "guest",
            "arch",
            "created",
            "created_from",
            "expires",
            "owner",
        ];
        let positions: Vec<usize> = keys
            .iter()
            .map(|key| line.find(&format!("\"{key}\":")).unwrap())
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "keys out of order: {line}"
        );
        assert_eq!(
            serde_json::from_str::<Value>(line).unwrap().as_object().unwrap().len(),
            keys.len()
        );
    }
    assert_eq!(objects[0]["owner"], "none");
    assert_eq!(objects[1]["owner"], "process");

    let from_b = stdout_lines(&home.devvm(&b, &["ls", "--json"]));
    drop(owner);
    assert_eq!(from_b.len(), 1);
    assert_eq!(serde_json::from_str::<Value>(&from_b[0]).unwrap()["name"], "b1");
}

#[skuld::test]
fn ls_all_lists_instances_from_other_directories() {
    let home = home();
    let (a, b) = (home.project("a"), home.project("b"));
    home.seed("a1", &id(1), &a, "none");
    home.seed("b1", &id(2), &b, "none");
    let output = home.devvm(&a, &["ls", "--all", "--json"]);
    let names: Vec<String> = stdout_lines(&output)
        .iter()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["name"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(names, ["a1", "b1"]);
}

#[skuld::test]
fn ls_prints_a_table_and_nothing_at_all_when_there_are_no_instances() {
    let home = home();
    let a = home.project("a");
    let empty = home.devvm(&a, &["ls"]);
    assert_eq!(empty.status.code(), Some(0));
    assert!(empty.stdout.is_empty() && empty.stderr.is_empty());

    home.seed("alpha", &id(1), &a, "none");
    home.seed("beta", &id(2), &a, "process");
    // A live owner: this process holds the owner lock, so the reaper leaves beta alone.
    let owner = std::fs::OpenOptions::new()
        .write(true)
        .open(home.state().join("locks").join(format!("instance-{}.owner", id(2))))
        .unwrap();
    owner.lock().unwrap();

    let lines = stdout_lines(&home.devvm(&a, &["ls"]));
    drop(owner);
    assert_eq!(lines.len(), 3, "{lines:?}");
    let header: Vec<&str> = lines[0].split_whitespace().collect();
    assert_eq!(header, ["NAME", "STATE", "GUEST", "ARCH", "OWNER"]);
    // Every column of each row, in header order.
    let rows: Vec<Vec<&str>> = lines[1..]
        .iter()
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(rows[0], ["alpha", "stopped", "linux", "amd64", "none"]);
    assert_eq!(rows[1], ["beta", "stopped", "linux", "amd64", "process"]);
}

/// Replaces the `machine` of the seeded instance `name` with a record of a process that no backend can look for.
fn seed_a_machine(home: &Home, name: &str) {
    let path = home.state().join("instances").join(name).join("record.json");
    let record = std::fs::read_to_string(&path).unwrap().replace(
        r#""machine": null"#,
        r#""machine": {
    "process": {"v": 1, "platform": "windows", "pid": 4242, "token": "133000000000000000"},
    "qmp": "",
    "ssh": [],
    "serial_log": "serial.log",
    "leftovers": []
  }"#,
    );
    std::fs::write(path, record).unwrap();
}

#[skuld::test]
fn ls_says_unknown_for_a_recorded_machine_this_build_has_no_backend_to_look_for() {
    let home = home();
    let a = home.project("a");
    home.seed("alpha", &id(1), &a, "none");
    seed_a_machine(&home, "alpha");

    let table = home.devvm(&a, &["ls"]);
    assert_eq!(table.status.code(), Some(0));
    let lines = stdout_lines(&table);
    let row: Vec<&str> = lines[1].split_whitespace().collect();
    assert_eq!(row, ["alpha", "unknown", "linux", "amd64", "none"]);
    let stderr = String::from_utf8(table.stderr).unwrap();
    assert!(
        stderr.contains("alpha") && stderr.contains("no VM backend"),
        "the cause is said: {stderr:?}"
    );

    let json = home.devvm(&a, &["ls", "--json"]);
    let object: Value = serde_json::from_str(&stdout_lines(&json)[0]).unwrap();
    assert_eq!(object["state"], "unknown");
}

#[skuld::test]
fn rm_removes_a_seeded_stopped_instance() {
    let home = home();
    let a = home.project("a");
    home.seed("gone", &id(1), &a, "none");
    home.seed("kept", &id(2), &a, "none");

    let output = home.devvm(&a, &["rm", "GONE"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let names: Vec<String> = stdout_lines(&home.devvm(&a, &["ls", "--json"]))
        .iter()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["name"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(names, ["kept"]);
    assert!(!home.state().join("instances").join("gone").exists());

    let again = home.devvm(&a, &["rm", "gone"]);
    assert_eq!(again.status.code(), Some(125));
    one_error_line(&again, "not-found");
    let bad = home.devvm(&a, &["rm", "../x"]);
    assert_eq!(bad.status.code(), Some(125));
    one_error_line(&bad, "invalid-name");
}

#[skuld::test]
fn every_command_reaps_first() {
    let home = home();
    let a = home.project("a");
    // A creator that died before it placed its instance: its temp directory is left, and no lock file exists.
    let leftover = home.state().join("instances").join(format!(".new-{}", id(9)));
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("disk.img"), "x").unwrap();

    let output = home.devvm(&a, &["ls"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(!leftover.exists());
}

#[skuld::test]
fn commands_that_need_a_backend_exit_125_not_supported() {
    let home = home();
    let a = home.project("a");
    let commands: [&[&str]; 8] = [
        &["run", "ubuntu-24.04", "--", "true"],
        &["up", "ubuntu-24.04", "--name", "box"],
        &["ssh", "box"],
        &["cp", "a.txt", "box:~/a.txt"],
        &["start", "box"],
        &["stop", "box"],
        &["build", "ubuntu-24.04"],
        &["doctor"],
    ];
    for args in commands {
        let output = home.devvm(&a, args);
        assert_eq!(output.status.code(), Some(125), "{args:?}");
        one_error_line(&output, "not-supported");
    }
}

#[skuld::test]
fn a_usage_error_exits_125_with_a_status_file() {
    let home = home();
    let a = home.project("a");
    let status = home.root.join("status.json");
    let output = home.devvm(&a, &["--status-file", status.to_str().unwrap(), "run", "ubuntu-24.04"]);
    assert_eq!(output.status.code(), Some(125));
    one_error_line(&output, "usage");
    let report: Value = serde_json::from_str(&std::fs::read_to_string(&status).unwrap()).unwrap();
    assert_eq!(report["devvm"]["kind"], "usage");
}
