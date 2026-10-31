//! The TCP stream of a session, watched for its end.
//!
//! russh tells the client handler when a connection ends, but not on every path: a session task that fails in its
//! shutdown after a reset (measured: `ENOTCONN` on Linux) returns without calling `Handler::disconnected`, and a
//! request in flight would then wait for a reply that cannot come. The stream sees every end, so it records the end
//! itself.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::Transport;

#[cfg(test)]
#[path = "watched_tests.rs"]
mod watched_tests;

/// `S`, with a read of end-of-file or a failed read or write recorded as the end of `transport`'s connection.
pub(super) struct Watched<S> {
    inner: S,
    transport: Arc<Transport>,
}

impl<S> Watched<S> {
    pub(super) fn new(inner: S, transport: Arc<Transport>) -> Watched<S> {
        Watched { inner, transport }
    }

    fn failed<T>(&self, doing: &str, result: Poll<io::Result<T>>) -> Poll<io::Result<T>> {
        if let Poll::Ready(Err(e)) = &result {
            self.transport.end(format!("the connection failed ({doing}: {e})"));
        }
        result
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let asked = buf.remaining();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        // A read into no space returns nothing without meaning the end.
        if asked > 0 && matches!(result, Poll::Ready(Ok(()))) && buf.filled().len() == before {
            self.transport.end("the peer closed the connection".to_owned());
        }
        self.failed("reading", result)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        self.failed("writing", result)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        self.failed("writing", result)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.inner).poll_shutdown(cx);
        self.failed("shutting down", result)
    }
}
