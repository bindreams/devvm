//! A process watch that can be made to refuse: the OS declining to say whether a process has exited.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::backend::qemu::uds::UnixStream;
use crate::host::{Link, ProcessWatch};
use crate::{Error, ErrorKind, Result};

/// What a refusing call fails with.
pub const REFUSED: &str = "the test watch was made to refuse";

/// Delegates to `inner` until told to refuse; `has_exited` and `pause` then fail with [`REFUSED`] (`Internal`), and
/// `link` with an I/O error. A watch that refuses from the start is a tripwire: whatever consults it has connected to
/// QEMU.
pub struct Refusing {
    inner: Arc<dyn ProcessWatch>,
    has_exited: AtomicBool,
    pause: AtomicBool,
    link: AtomicBool,
    /// Reads on every link fail with this kind of error.
    read_error: Arc<Mutex<Option<std::io::ErrorKind>>>,
    /// Writes that contain the text fail with the kind of error.
    write_error: Arc<Mutex<Option<(String, std::io::ErrorKind)>>>,
}

impl Refusing {
    pub fn new(inner: Arc<dyn ProcessWatch>) -> Arc<Refusing> {
        Arc::new(Refusing {
            inner,
            has_exited: AtomicBool::new(false),
            pause: AtomicBool::new(false),
            link: AtomicBool::new(false),
            read_error: Arc::default(),
            write_error: Arc::default(),
        })
    }

    /// The watch, for the code under test.
    pub fn watch(self: &Arc<Self>) -> Arc<dyn ProcessWatch> {
        self.clone()
    }

    pub fn refuse_has_exited(&self) {
        self.has_exited.store(true, Ordering::SeqCst);
    }

    pub fn refuse_pause(&self) {
        self.pause.store(true, Ordering::SeqCst);
    }

    pub fn refuse_link(&self) {
        self.link.store(true, Ordering::SeqCst);
    }

    /// Makes every read, on links made already too, fail with an error of `kind`.
    pub fn fail_reads_with(&self, kind: std::io::ErrorKind) {
        *self.read_error.lock().unwrap() = Some(kind);
    }

    /// Makes every write that contains `needle`, on links made already too, fail with an error of `kind`: the one
    /// request of a session that a test wants refused, whatever came before it.
    pub fn fail_writes_containing(&self, needle: &str, kind: std::io::ErrorKind) {
        *self.write_error.lock().unwrap() = Some((needle.to_owned(), kind));
    }
}

fn refusal() -> Error {
    Error::new(ErrorKind::Internal, REFUSED)
}

impl ProcessWatch for Refusing {
    fn has_exited(&self) -> Result<bool> {
        if self.has_exited.load(Ordering::SeqCst) {
            return Err(refusal());
        }
        self.inner.has_exited()
    }

    fn pause(&self) -> Result<bool> {
        if self.pause.load(Ordering::SeqCst) {
            return Err(refusal());
        }
        self.inner.pause()
    }

    fn link(&self, stream: UnixStream) -> std::io::Result<Box<dyn Link>> {
        if self.link.load(Ordering::SeqCst) {
            return Err(std::io::Error::other(REFUSED));
        }
        Ok(Box::new(FailingLink {
            link: self.inner.link(stream)?,
            read_error: self.read_error.clone(),
            write_error: self.write_error.clone(),
        }))
    }
}

/// A link whose reads and writes fail once `Refusing::fail_reads_with` and `Refusing::fail_writes_containing` say so.
struct FailingLink {
    link: Box<dyn Link>,
    read_error: Arc<Mutex<Option<std::io::ErrorKind>>>,
    write_error: Arc<Mutex<Option<(String, std::io::ErrorKind)>>>,
}

impl Read for FailingLink {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match *self.read_error.lock().unwrap() {
            Some(kind) => Err(kind.into()),
            None => self.link.read(buf),
        }
    }
}

impl Write for FailingLink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some((needle, kind)) = &*self.write_error.lock().unwrap()
            && String::from_utf8_lossy(buf).contains(needle.as_str())
        {
            return Err((*kind).into());
        }
        self.link.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.link.flush()
    }
}
