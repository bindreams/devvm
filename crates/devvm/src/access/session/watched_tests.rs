use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use super::Watched;
use crate::access::runtime;
use crate::access::session::Transport;

fn reason(transport: &Transport) -> Option<String> {
    transport.ended.borrow().clone()
}

/// A stream whose every read and write fails, as a socket does after a reset.
struct Reset;

impl AsyncRead for Reset {
    fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from(io::ErrorKind::ConnectionReset)))
    }
}

impl AsyncWrite for Reset {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A stream that carries data and fails only when shut down, as a socket does after a reset that came once the session
/// had stopped reading.
struct FailsToShutDown;

impl AsyncRead for FailsToShutDown {
    fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Poll::Pending
    }
}

impl AsyncWrite for FailsToShutDown {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Err(io::Error::from(io::ErrorKind::NotConnected)))
    }
}

#[skuld::test]
fn a_failed_shutdown_is_the_end_of_the_connection() {
    let transport = Transport::new();
    runtime::run("a watched failed shutdown", {
        let transport = transport.clone();
        async move {
            let mut watched = Watched::new(FailsToShutDown, transport);
            watched.write_all(b"x").await.unwrap();
            assert!(watched.shutdown().await.is_err());
        }
    })
    .unwrap();
    let why = reason(&transport).expect("the end was recorded");
    assert!(why.contains("shutting down"), "{why}");
}

#[skuld::test]
fn data_passes_through_and_is_not_an_end() {
    let transport = Transport::new();
    let (ours, mut theirs) = tokio::io::duplex(64);
    runtime::run("a watched round trip", {
        let transport = transport.clone();
        async move {
            let mut watched = Watched::new(ours, transport);
            theirs.write_all(b"ping").await.unwrap();
            let mut got = [0u8; 4];
            watched.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"ping");
            // A read into no space returns nothing without meaning the end.
            assert_eq!(watched.read(&mut []).await.unwrap(), 0);
            watched.write_all(b"pong").await.unwrap();
            watched.flush().await.unwrap();
            let mut back = [0u8; 4];
            theirs.read_exact(&mut back).await.unwrap();
            assert_eq!(&back, b"pong");
        }
    })
    .unwrap();
    assert_eq!(reason(&transport), None);
}

#[skuld::test]
fn the_peer_closing_is_the_end_of_the_connection() {
    let transport = Transport::new();
    let (ours, theirs) = tokio::io::duplex(64);
    drop(theirs);
    runtime::run("a watched read of end-of-file", {
        let transport = transport.clone();
        async move {
            let mut watched = Watched::new(ours, transport);
            assert_eq!(watched.read(&mut [0u8; 8]).await.unwrap(), 0);
        }
    })
    .unwrap();
    let why = reason(&transport).expect("the end was recorded");
    assert!(why.contains("closed"), "{why}");
}

#[skuld::test]
fn a_failed_read_or_write_is_the_end_of_the_connection_and_the_first_reason_stays() {
    let transport = Transport::new();
    runtime::run("a watched failure", {
        let transport = transport.clone();
        async move {
            let mut watched = Watched::new(Reset, transport);
            assert!(watched.read(&mut [0u8; 8]).await.is_err());
            assert!(watched.write_all(b"x").await.is_err());
        }
    })
    .unwrap();
    let why = reason(&transport).expect("the end was recorded");
    assert!(why.contains("reading"), "the first failure is the reason: {why}");
}

#[skuld::test]
fn a_waiter_on_the_transport_wakes_when_the_stream_ends() {
    let transport = Transport::new();
    let (ours, theirs) = tokio::io::duplex(64);
    let why = runtime::run("a waiter on a watched stream", {
        let transport = transport.clone();
        async move {
            let waiting = tokio::spawn({
                let transport = transport.clone();
                async move { transport.ended().await }
            });
            let mut watched = Watched::new(ours, transport);
            drop(theirs);
            let _ = watched.read(&mut [0u8; 8]).await;
            waiting.await.unwrap()
        }
    })
    .unwrap();
    assert!(why.contains("closed"), "{why}");
}
