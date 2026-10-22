//! Cross-process behavior of the layer store, with real child processes.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Stdio};
use std::sync::Arc;

use crate::exit::StderrProgress;
use crate::store::layers::{BuildRequest, Built, LayerStore, SystemClock};
use crate::test_support::fixtures::anna_home;
use crate::test_support::helper;
use crate::{Dirs, Error, ErrorKind};

use super::layers_tests::{key, names};

/// Helper process: builds layer `args[1]` (hex) under the store rooted at `args[0]`. In its builder it writes a marker
/// file `<args[2]>/built-<pid>`, prints `building <staging path>`, and waits for a `go` line on stdin (EOF abandons
/// the build). After `build` returns it prints `done <generation>`.
pub fn layer_builder(args: &[String]) {
    let dirs = Dirs::under(Path::new(&args[0]));
    let key = args[1].parse().expect("a layer key");
    let markers = Path::new(&args[2]);
    let store = LayerStore::open(&dirs, Arc::new(SystemClock), Arc::new(StderrProgress)).unwrap();
    let request = BuildRequest {
        key,
        source_label: "layer-builder helper".into(),
    };
    let lease = store
        .build(&request, None, |staging| {
            std::fs::write(markers.join(format!("built-{}", std::process::id())), b"").unwrap();
            println!("building {}", staging.dir.path().display());
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).unwrap() == 0 {
                return Err(Error::new(ErrorKind::Internal, "abandoned: stdin closed"));
            }
            std::fs::write(staging.dir.path().join("disk.qcow2"), b"disk").unwrap();
            Ok(Built { expires: None })
        })
        .unwrap();
    println!("done {}", lease.tip().meta().generation);
}

struct Builder {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
}

fn start(home: &Path, key_hex: &str, markers: &Path) -> Builder {
    let mut child = helper::command(
        "layer-builder",
        &[home.to_str().unwrap(), key_hex, markers.to_str().unwrap()],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    Builder {
        stdin: child.stdin.take().unwrap(),
        stdout: BufReader::new(child.stdout.take().unwrap()),
        stderr: BufReader::new(child.stderr.take().unwrap()),
        child,
    }
}

impl Builder {
    /// Reads stdout until a line starting with `prefix`; returns the rest of it.
    fn expect_stdout(&mut self, prefix: &str) -> String {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "the helper exited before printing `{prefix}`");
            if let Some(rest) = line.trim_end().strip_prefix(prefix) {
                return rest.trim().to_owned();
            }
        }
    }

    /// Reads stderr until a line containing `needle`.
    fn expect_stderr(&mut self, needle: &str) {
        loop {
            let mut line = String::new();
            let n = self.stderr.read_line(&mut line).unwrap();
            assert!(n > 0, "the helper exited before printing `{needle}` on stderr");
            if line.contains(needle) {
                return;
            }
        }
    }

    fn go(&mut self) {
        writeln!(self.stdin, "go").unwrap();
        self.stdin.flush().unwrap();
    }
}

#[skuld::test]
fn two_processes_building_one_layer_run_one_builder(#[fixture(anna_home)] home: &Path) {
    let markers = home.join("markers");
    std::fs::create_dir(&markers).unwrap();
    let key_hex = key(1).to_string();

    let mut a = start(home, &key_hex, &markers);
    a.expect_stdout("building ");
    let mut b = start(home, &key_hex, &markers);
    b.expect_stderr("waiting for another devvm process building layer");

    a.go();
    let a_generation = a.expect_stdout("done ");
    let b_generation = b.expect_stdout("done ");
    assert_eq!(a_generation, b_generation);
    assert!(a.child.wait().unwrap().success() && b.child.wait().unwrap().success());
    assert_eq!(names(&markers).len(), 1, "{:?}", names(&markers));
}

#[skuld::test]
fn killing_the_builder_lets_the_waiter_build_in_a_fresh_staging_dir(#[fixture(anna_home)] home: &Path) {
    let markers = home.join("markers");
    std::fs::create_dir(&markers).unwrap();
    let key_hex = key(1).to_string();

    let mut a = start(home, &key_hex, &markers);
    let a_staging = a.expect_stdout("building ");
    let mut b = start(home, &key_hex, &markers);
    b.expect_stderr("waiting for another devvm process building layer");

    a.child.kill().unwrap();
    a.child.wait().unwrap();
    assert!(
        Path::new(&a_staging).exists(),
        "the dead builder leaves its staging dir behind"
    );

    let b_staging = b.expect_stdout("building ");
    assert_ne!(a_staging, b_staging);
    assert!(
        !Path::new(&a_staging).exists(),
        "the waiter sweeps the dead builder's staging dir"
    );
    b.go();
    let generation = b.expect_stdout("done ");
    assert!(b.child.wait().unwrap().success());

    let store = LayerStore::open(&Dirs::under(home), Arc::new(SystemClock), Arc::new(StderrProgress)).unwrap();
    let hit = store.lookup(&key(1)).unwrap().expect("the waiter's layer is published");
    assert_eq!(hit.tip().meta().generation.to_string(), generation);
    assert_eq!(
        std::fs::read(hit.tip().dir().path().join("disk.qcow2")).unwrap(),
        b"disk"
    );
    assert_eq!(names(&markers).len(), 2, "both processes started a builder");
}
