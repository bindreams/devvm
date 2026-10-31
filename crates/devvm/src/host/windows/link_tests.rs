//! The link against a peer thread of this process: no QEMU process and no child process, so these run as often as one
//! likes. The "process" is an event the test sets (`ManualExit`); the peer holds its end open throughout, which is the
//! stream whose end Windows can lose.

use std::io::{Read, Write};
use std::sync::mpsc;

use crate::backend::qemu::uds::{UnixListener, UnixStream};
use crate::test_support::manual_exit::ManualExit;
use crate::test_support::qemu::Sock;

/// A connection: the end the link wraps, and the peer's end.
fn pair() -> (Sock, UnixStream, UnixStream) {
    let sock = Sock::new();
    let listener = UnixListener::bind(sock.endpoint.path()).unwrap();
    let client = UnixStream::connect(sock.endpoint.path()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    (sock, client, peer)
}

/// Reads `size` bytes from `peer`, 4 KiB at a time, and returns how many came.
fn drain(mut peer: impl Read, size: usize) -> usize {
    let mut got = 0;
    let mut buffer = [0u8; 4096];
    while got < size {
        match peer.read(&mut buffer).unwrap() {
            0 => break,
            n => got += n,
        }
    }
    got
}

/// Everything `link` reads until it ends the stream.
fn read_all(mut link: impl Read) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        match link.read(&mut buffer).unwrap() {
            0 => return got,
            n => got.extend_from_slice(&buffer[..n]),
        }
    }
}

#[skuld::test]
fn a_reader_blocked_when_the_process_exits_returns_every_byte_the_peer_wrote() {
    let exit = ManualExit::new();
    let (_sock, client, mut peer) = pair();
    let mut link = exit.watch().link(client).unwrap();
    peer.write_all(b"one\ntwo\n").unwrap();
    let (has_bytes, has_bytes_rx) = mpsc::channel::<()>();

    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match link.read(&mut buffer).unwrap() {
                0 => return got,
                n => got.extend_from_slice(&buffer[..n]),
            }
            // The bytes are in; the next read has nothing to return until the exit.
            has_bytes.send(()).unwrap();
        }
    });
    has_bytes_rx.recv().unwrap();
    exit.exit();

    assert_eq!(reader.join().unwrap(), b"one\ntwo\n");
    drop(peer);
}

#[skuld::test]
fn bytes_that_arrive_together_with_the_exit_are_all_read_before_the_end_is_reported() {
    let exit = ManualExit::new();
    let (_sock, client, mut peer) = pair();
    let link = exit.watch().link(client).unwrap();
    peer.write_all(b"last words\n").unwrap();
    exit.exit();

    assert_eq!(read_all(link), b"last words\n");
    drop(peer);
}

#[skuld::test]
fn a_link_made_after_the_exit_still_reads_what_is_there_and_then_ends() {
    let exit = ManualExit::new();
    exit.exit();
    let (_sock, client, mut peer) = pair();
    peer.write_all(b"late\n").unwrap();
    let link = exit.watch().link(client).unwrap();

    assert_eq!(read_all(link), b"late\n");
    drop(peer);
}

#[skuld::test]
fn more_than_the_socket_buffer_holds_is_all_read_when_the_exit_follows_the_last_write() {
    const SIZE: usize = 4 * 1024 * 1024;
    let exit = ManualExit::new();
    let (_sock, client, mut peer) = pair();
    let link = exit.watch().link(client).unwrap();

    let writer = std::thread::spawn(move || {
        peer.write_all(&vec![b'x'; SIZE]).unwrap();
        // Held open until the test is over: the exit is the only end there is.
        peer
    });
    let reader = std::thread::spawn(move || read_all(link));
    let peer = writer.join().unwrap();
    exit.exit();

    assert_eq!(reader.join().unwrap().len(), SIZE);
    drop(peer);
}

#[skuld::test]
fn a_link_whose_process_has_not_exited_carries_both_directions() {
    let exit = ManualExit::new();
    let (_sock, client, mut peer) = pair();
    let mut link = exit.watch().link(client).unwrap();

    link.write_all(b"ping\n").unwrap();
    let mut got = [0u8; 5];
    peer.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"ping\n");

    // The reader parks until the peer answers, then wakes for the data alone.
    let reader = std::thread::spawn(move || {
        let mut got = [0u8; 5];
        link.read_exact(&mut got).unwrap();
        got
    });
    peer.write_all(b"pong\n").unwrap();
    assert_eq!(&reader.join().unwrap(), b"pong\n");
}

#[skuld::test]
fn a_write_that_cannot_finish_when_the_process_exits_fails_as_a_closed_connection() {
    let exit = ManualExit::new();
    let (_sock, client, peer) = pair();
    let mut link = exit.watch().link(client).unwrap();

    // The peer never reads, so a write this large fills the buffers and waits for room; or the exit comes first, and
    // the write finds the buffers full all the same.
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        let chunk = [b'x'; 64 * 1024];
        loop {
            link.write_all(&chunk)?;
        }
    });
    exit.exit();

    let error: std::io::Error = writer.join().unwrap().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
    drop(peer);
}

#[skuld::test]
fn a_write_larger_than_the_socket_buffers_goes_through_as_the_peer_drains() {
    const SIZE: usize = 16 * 1024 * 1024;
    let exit = ManualExit::new();
    let (_sock, client, peer) = pair();
    let mut link = exit.watch().link(client).unwrap();

    // The writer hands over far more than the buffers hold, in chunks (Winsock takes one huge send whole), and the peer
    // takes 4 KiB at a time: the writer finds the socket full again and again and goes on only when the peer's reads
    // make room.
    let writer = std::thread::spawn(move || {
        let chunk = [b'x'; 64 * 1024];
        (0..SIZE / chunk.len()).try_for_each(|_| link.write_all(&chunk))
    });
    let received = drain(peer, SIZE);

    writer.join().unwrap().unwrap();
    assert_eq!(received, SIZE);
}

#[skuld::test]
fn small_reads_that_leave_bytes_behind_and_then_run_dry_wake_for_the_next_bytes() {
    let exit = ManualExit::new();
    let (_sock, client, mut peer) = pair();
    let mut link = exit.watch().link(client).unwrap();
    let (drained, drained_rx) = mpsc::channel::<()>();

    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        for burst in [10usize, 7] {
            let mut left = burst;
            while left > 0 {
                // Three bytes at a time: most reads stop short of what the socket holds.
                let mut buffer = [0u8; 3];
                let n = link.read(&mut buffer[..left.min(3)]).unwrap();
                got.extend_from_slice(&buffer[..n]);
                left -= n;
            }
            drained.send(()).unwrap();
        }
        got
    });
    peer.write_all(b"0123456789").unwrap();
    drained_rx.recv().unwrap();
    peer.write_all(b"abcdefg").unwrap();
    drained_rx.recv().unwrap();

    assert_eq!(reader.join().unwrap(), b"0123456789abcdefg");
    drop(peer);
}
