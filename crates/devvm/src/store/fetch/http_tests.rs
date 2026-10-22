use std::time::Duration;

use super::ContentRange;
use super::http::{parse_content_range, read_head, stall_message};

#[skuld::test]
fn stall_error_text_names_url_and_seconds() {
    assert_eq!(
        stall_message("https://h/disk.img", Duration::from_secs(60)),
        "no data from https://h/disk.img for 60s"
    );
}

#[skuld::test]
fn content_range_headers_parse() {
    assert_eq!(
        parse_content_range("bytes 100-199/1000"),
        Some(ContentRange::Range {
            start: 100,
            end: 199,
            total: Some(1000)
        })
    );
    assert_eq!(
        parse_content_range("bytes 0-9/*"),
        Some(ContentRange::Range {
            start: 0,
            end: 9,
            total: None
        })
    );
    assert_eq!(
        parse_content_range("bytes */1000"),
        Some(ContentRange::Unsatisfied { total: 1000 })
    );
    for bad in [
        "",
        "bytes",
        "bytes 1-2",
        "bytes a-b/c",
        "items 1-2/3",
        "bytes */*",
        "bytes 5/10",
    ] {
        assert_eq!(parse_content_range(bad), None, "{bad:?}");
    }
}

// Over real HTTP ======================================================================================================

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, Stdio};
use std::sync::Arc;
use std::sync::mpsc;

use sha2::{Digest, Sha256 as Sha2};

use super::{Artifact, Fetcher, HttpTransport};
use crate::exit::{NullProgress, StderrProgress};
use crate::test_support::fixtures::anna_home;
use crate::test_support::helper;
use crate::test_support::http::{Behavior, TestServer};
use crate::{Dirs, ErrorKind, Sha256};

const ETAG: &str = "\"etag-1\"";

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn artifact_for(server: &TestServer, data: &[u8]) -> Artifact {
    Artifact {
        url: server.url("/files/disk.img"),
        sha256: Sha256::from_bytes(Sha2::digest(data).into()),
    }
}

/// Helper process: fetches `args[1]` (pinned to sha256 `args[2]`) into downloads dir `args[0]` over real HTTP,
/// narrating on stderr, then prints `done <path>`.
pub fn fetch_artifact(args: &[String]) {
    let artifact = Artifact {
        url: args[1].clone(),
        sha256: args[2].parse().unwrap(),
    };
    let transport = Arc::new(HttpTransport::new().unwrap());
    let fetcher = Fetcher::new(args[0].clone().into(), transport, Arc::new(StderrProgress));
    let path = fetcher.fetch(&artifact).unwrap();
    println!("done {}", path.display());
}

struct Child2 {
    child: Child,
    stdout: BufReader<ChildStdout>,
    stderr: BufReader<ChildStderr>,
}

impl Child2 {
    fn start(downloads: &Path, artifact: &Artifact) -> Child2 {
        let mut child = helper::command(
            "fetch-artifact",
            &[downloads.to_str().unwrap(), &artifact.url, &artifact.sha256.to_string()],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        Child2 {
            stdout: BufReader::new(child.stdout.take().unwrap()),
            stderr: BufReader::new(child.stderr.take().unwrap()),
            child,
        }
    }

    /// Reads stderr until a line containing `needle`; returns that line.
    fn expect_stderr(&mut self, needle: &str) -> String {
        loop {
            let mut line = String::new();
            assert!(
                self.stderr.read_line(&mut line).unwrap() > 0,
                "the child exited before printing `{needle}`"
            );
            if line.contains(needle) {
                return line;
            }
        }
    }

    fn expect_done(&mut self) -> String {
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "the child exited without `done`"
            );
            if let Some(path) = line.trim_end().strip_prefix("done ") {
                return path.to_owned();
            }
        }
    }
}

#[skuld::test]
fn http_transport_resumes_with_range_and_if_range(#[fixture(anna_home)] home: &Path) {
    let body = data(300_000);
    let server = TestServer::start(body.clone(), ETAG);
    server.script(vec![Behavior::AbortAfter(81_920)]);
    let artifact = artifact_for(&server, &body);
    let downloads = Dirs::under(home).downloads();
    let fetcher = Fetcher::new(
        downloads.clone(),
        Arc::new(HttpTransport::new().unwrap()),
        Arc::new(NullProgress),
    );

    let error = fetcher.fetch(&artifact).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Download, "{error}");
    let part = downloads.join(format!("{}.part", artifact.sha256));
    let kept = std::fs::metadata(&part).unwrap().len();
    assert!(kept > 0 && kept <= 81_920, "{kept}");

    let path = fetcher.fetch(&artifact).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), body);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].range.as_deref(), Some(format!("bytes={kept}-").as_str()));
    assert_eq!(requests[1].if_range.as_deref(), Some(ETAG));
}

#[skuld::test]
fn concurrent_first_fetch_downloads_once(#[fixture(anna_home)] home: &Path) {
    let body = data(1_000_000);
    let server = TestServer::start(body.clone(), ETAG);
    let (paused_tx, paused_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    server.script(vec![Behavior::PauseAfter {
        after: 65_536,
        paused: paused_tx,
        release: release_rx,
    }]);
    let artifact = artifact_for(&server, &body);
    let downloads = Dirs::under(home).downloads();

    let mut a = Child2::start(&downloads, &artifact);
    a.expect_stderr("downloading");
    paused_rx.recv().unwrap();
    let mut b = Child2::start(&downloads, &artifact);
    b.expect_stderr("waiting for another devvm process");

    release_tx.send(()).unwrap();
    let (a_path, b_path) = (a.expect_done(), b.expect_done());
    assert_eq!(a_path, b_path);
    assert_eq!(std::fs::read(&a_path).unwrap(), body);
    assert!(a.child.wait().unwrap().success() && b.child.wait().unwrap().success());
    assert_eq!(server.requests().iter().filter(|r| r.method == "GET").count(), 1);
}

#[skuld::test]
fn ctrl_c_mid_download_resumes(#[fixture(anna_home)] home: &Path) {
    const MIB: usize = 1024 * 1024;
    let body = data(4 * MIB + 123);
    let server = TestServer::start(body.clone(), ETAG);
    let (paused_tx, paused_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    server.script(vec![Behavior::PauseAfter {
        after: 3 * MIB,
        paused: paused_tx,
        release: release_rx,
    }]);
    let artifact = artifact_for(&server, &body);
    let downloads = Dirs::under(home).downloads();

    let mut a = Child2::start(&downloads, &artifact);
    a.expect_stderr("3145728");
    paused_rx.recv().unwrap();
    // Process death is the Ctrl-C equivalent on every OS; a console handler is the CLI's business.
    a.child.kill().unwrap();
    a.child.wait().unwrap();

    let part = downloads.join(format!("{}.part", artifact.sha256));
    let kept = std::fs::metadata(&part).unwrap().len();
    assert!(kept >= (3 * MIB) as u64, "{kept}");

    let fetcher = Fetcher::new(
        downloads.clone(),
        Arc::new(HttpTransport::new().unwrap()),
        Arc::new(NullProgress),
    );
    // The dead downloader's lock died with it: this fetch does not wait.
    let path = fetcher.fetch(&artifact).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), body);
    let requests = server.requests();
    assert_eq!(
        requests.last().unwrap().range.as_deref(),
        Some(format!("bytes={kept}-").as_str())
    );
    drop(release_tx);
}

#[skuld::test]
fn a_stall_mid_body_is_reported_as_a_stall_and_keeps_the_part(#[fixture(anna_home)] home: &Path) {
    let body = data(300_000);
    let server = TestServer::start(body.clone(), ETAG);
    let (paused_tx, paused_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    server.script(vec![Behavior::PauseAfter {
        after: 81_920,
        paused: paused_tx,
        release: release_rx,
    }]);
    let artifact = artifact_for(&server, &body);
    let downloads = Dirs::under(home).downloads();
    let transport = HttpTransport::with_stall(Duration::from_secs(1)).unwrap();
    let fetcher = Fetcher::new(downloads.clone(), Arc::new(transport), Arc::new(NullProgress));

    let error = fetcher.fetch(&artifact).unwrap_err();
    paused_rx.recv().unwrap();
    drop(release_tx);
    assert_eq!(error.kind(), ErrorKind::Download, "{error}");
    assert!(
        error
            .message()
            .contains(&format!("no data from {} for 1s", artifact.url)),
        "{error}"
    );
    let part = downloads.join(format!("{}.part", artifact.sha256));
    assert!(std::fs::metadata(part).unwrap().len() > 0);
}

#[skuld::test]
fn a_weak_etag_is_never_sent_as_if_range(#[fixture(anna_home)] home: &Path) {
    let body = data(300_000);
    let server = TestServer::start(body.clone(), "W/\"weak-1\"");
    server.script(vec![Behavior::AbortAfter(81_920)]);
    let artifact = artifact_for(&server, &body);
    let fetcher = Fetcher::new(
        Dirs::under(home).downloads(),
        Arc::new(HttpTransport::new().unwrap()),
        Arc::new(NullProgress),
    );

    assert_eq!(fetcher.fetch(&artifact).unwrap_err().kind(), ErrorKind::Download);
    let path = fetcher.fetch(&artifact).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), body);
    let requests = server.requests();
    assert!(requests[1].range.is_some(), "the resume still asks for a range");
    assert_eq!(requests[1].if_range, None, "a weak validator can never match If-Range");
}

fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    map
}

#[skuld::test]
fn the_validator_is_a_strong_etag_else_last_modified_else_none() {
    let modified = "Wed, 21 Oct 2015 07:28:00 GMT";
    type Case<'a> = (&'a [(&'a str, &'a str)], Option<&'a str>);
    let cases: [Case; 5] = [
        (&[("etag", "\"v1\"")], Some("\"v1\"")),
        (&[("etag", "\"v1\""), ("last-modified", modified)], Some("\"v1\"")),
        (&[("etag", "W/\"v1\""), ("last-modified", modified)], Some(modified)),
        (&[("etag", "W/\"v1\"")], None),
        (&[], None),
    ];
    for (pairs, expected) in cases {
        let head = read_head(&headers(pairs));
        assert_eq!(head.validator.as_deref(), expected, "{pairs:?}");
        assert!(head.warnings.is_empty(), "{pairs:?}: {:?}", head.warnings);
    }
}

#[skuld::test]
fn malformed_length_and_range_headers_are_named_in_warnings() {
    let head = read_head(&headers(&[("content-length", "lots"), ("content-range", "bytes 5")]));
    assert_eq!((head.content_length, head.content_range), (None, None));
    let [range, length] = head.warnings.try_into().unwrap();
    assert!(length.contains("Content-Length") && length.contains("lots"), "{length}");
    assert!(range.contains("Content-Range") && range.contains("bytes 5"), "{range}");

    let head = read_head(&headers(&[("content-length", "10"), ("content-range", "bytes 0-9/10")]));
    assert_eq!(head.content_length, Some(10));
    assert!(head.warnings.is_empty());
}
