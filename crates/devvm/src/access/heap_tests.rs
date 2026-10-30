//! Tests of how much heap the access layer holds. The number only means something in a process of its own (the shared
//! test binary runs many tests at once), so each test runs the lib's test binary as the helper `access-heap` and asserts
//! that it exited 0; the helper does the work under the counting allocator of `test_support::alloc` and exits non-zero
//! when the peak is too high.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::GuestKind;
use crate::access::ExecEnd;
use crate::exit::ExitStatus;
use crate::guest::Guests;
use crate::guest::fake::FakeGuests;
use crate::test_support::alloc::peak_bytes;
use crate::test_support::fixtures::MemoryProgress;
use crate::test_support::helper;
use crate::test_support::host::TestHost;
use crate::test_support::sshd::{BIG_SIZE, Rig, big_file_digest, expected_big_digest, pattern_byte, sample_windows};

/// The most heap the helper may have held at once.
const LIMIT: usize = 64 << 20;

/// The helper's entry point: `access-heap <scenario>`.
pub fn access_heap(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("slow-writer") => slow_writer(),
        Some("big-copy") => big_copy(),
        other => panic!("unknown access-heap scenario {other:?}"),
    }
}

fn run_helper(scenario: &str) {
    let output = helper::command("access-heap", &[scenario])
        .output()
        .expect("the helper starts");
    assert!(
        output.status.success(),
        "access-heap {scenario} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[skuld::test]
fn a_slow_writer_does_not_make_the_session_buffer_the_stream() {
    run_helper("slow-writer");
}

#[skuld::test]
fn a_4_gib_plus_one_file_streams_in_both_directions_without_buffering() {
    run_helper("big-copy");
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

// Scenario: a slow writer =============================================================================================

/// A host writer slower than the server: 40 MiB/s. Its pace is the load of the test, not a synchronisation: the
/// assertion is on the heap, and a session that buffered the stream holds most of it.
struct ThrottledSink {
    started: Instant,
    written: u64,
}

impl ThrottledSink {
    const BYTES_PER_SECOND: u64 = 40 << 20;
}

impl Write for ThrottledSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written += buf.len() as u64;
        let due = Duration::from_secs_f64(self.written as f64 / Self::BYTES_PER_SECOND as f64);
        if let Some(wait) = due.checked_sub(self.started.elapsed()) {
            std::thread::sleep(wait);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The server writes 320 MiB (a fifth of it is the limit); the writer takes it at 40 MiB/s.
fn slow_writer() {
    const MIB: u64 = 320;
    let dir = scratch();
    let rig = Rig::new(dir.path());
    let session = rig.connect();
    let mut sink = ThrottledSink {
        started: Instant::now(),
        written: 0,
    };
    let end = session
        .exec(&format!("stdout-mib {MIB}"), &mut sink, &mut Vec::new())
        .expect("the stream arrives");
    assert!(matches!(end, ExecEnd::Finished(ExitStatus::Code(0))), "{end:?}");
    assert_eq!(sink.written, MIB << 20);
    let peak = peak_bytes();
    assert!(peak < LIMIT, "peak heap {peak} bytes is not below {LIMIT}");
}

// Scenario: a file past 4 GiB =========================================================================================

/// A host file of `BIG_SIZE` bytes: zeros, with the pattern in the sample windows.
fn make_big_source(path: &Path) {
    let mut file = std::fs::File::create(path).expect("the big source file");
    file.set_len(BIG_SIZE).expect("a file of 4 GiB + 1 bytes");
    for (start, end) in sample_windows() {
        file.seek(SeekFrom::Start(start)).unwrap();
        file.write_all(&(start..end).map(pattern_byte).collect::<Vec<_>>())
            .unwrap();
    }
}

fn big_copy() {
    let dir = scratch();
    let rig = Rig::new(dir.path());
    std::fs::create_dir(rig.sshd.root().join("big")).unwrap();
    rig.sshd.discard_uploads_in("/big");
    rig.sshd.add_generated_file("/big/out.bin", BIG_SIZE);
    let session = rig.connect();
    let sftp = session.sftp().unwrap();
    let guest = FakeGuests.os(GuestKind::Linux).unwrap();
    let progress = MemoryProgress::default();

    let source = dir.path().join("source.bin");
    make_big_source(&source);
    sftp.copy_in(&source, "/big/in.bin", &TestHost, &*guest, &progress)
        .expect("the upload succeeds");
    std::fs::remove_file(&source).unwrap();
    assert_eq!(
        rig.sshd.sink_result("/big/in.bin"),
        Some((BIG_SIZE, expected_big_digest())),
        "the guest received every byte of the sampled windows"
    );

    let target = dir.path().join("out.bin");
    sftp.copy_out("/big/out.bin", &target, &TestHost, &*guest, &progress)
        .expect("the download succeeds");
    assert_eq!(std::fs::metadata(&target).unwrap().len(), BIG_SIZE);
    assert_eq!(big_file_digest(&target).unwrap(), expected_big_digest());
    drop(session);

    let peak = peak_bytes();
    assert!(peak < LIMIT, "peak heap {peak} bytes is not below {LIMIT}");
}
