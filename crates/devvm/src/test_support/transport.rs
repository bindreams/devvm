//! A transport that downloads nothing.

use std::sync::Mutex;

use crate::store::fetch::{Response, Resume, Transport};
use crate::{Error, ErrorKind, Result};

/// Answers every request with a download error naming the URL, and remembers the URLs.
#[derive(Default)]
pub struct RefusingTransport(Mutex<Vec<String>>);

impl RefusingTransport {
    /// The URLs asked for so far, in order.
    pub fn asked(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Transport for RefusingTransport {
    fn get(&self, url: &str, _resume: Option<Resume>) -> Result<Response> {
        self.0.lock().unwrap().push(url.to_owned());
        Err(Error::new(
            ErrorKind::Download,
            format!("no network in this test: {url}"),
        ))
    }
}
