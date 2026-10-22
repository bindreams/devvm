//! The real transport: blocking HTTP over rustls, bounded only by stalls.

use std::io::Read;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};

use super::{ContentRange, Response, Resume, Transport};
use crate::{Error, ErrorKind, Result};

/// A connection attempt, or a wait for the next bytes, that gets nothing for this long fails. There is no bound on
/// the whole download: a slow link that keeps delivering is fine.
const STALL: Duration = Duration::from_secs(60);

pub struct HttpTransport {
    client: reqwest::blocking::Client,
}

impl HttpTransport {
    pub fn new() -> Result<HttpTransport> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(STALL)
            // On the blocking client this bounds the wait for the response head and each body read, not the transfer.
            .timeout(STALL)
            .user_agent(concat!("devvm/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| {
                Error::new(ErrorKind::Internal, format!("cannot build the HTTP client: {e}")).with_source(e)
            })?;
        Ok(HttpTransport { client })
    }
}

/// The text of a stall: what was awaited and for how long.
pub(super) fn stall_message(url: &str, stall: Duration) -> String {
    format!("no data from {url} for {}s", stall.as_secs())
}

/// Parses `bytes 100-199/1000`, `bytes 100-199/*` and `bytes */1000`.
pub(super) fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (span, total) = rest.split_once('/')?;
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse::<u64>().ok()?),
    };
    if span.trim() == "*" {
        return Some(ContentRange::Unsatisfied { total: total? });
    }
    let (start, end) = span.split_once('-')?;
    Some(ContentRange::Range {
        start: start.trim().parse().ok()?,
        end: end.trim().parse().ok()?,
        total,
    })
}

struct Body {
    inner: reqwest::blocking::Response,
    url: String,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf).map_err(|e| {
            if e.kind() == std::io::ErrorKind::TimedOut {
                std::io::Error::new(e.kind(), stall_message(&self.url, STALL))
            } else {
                e
            }
        })
    }
}

fn header(response: &reqwest::blocking::Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

impl Transport for HttpTransport {
    fn get(&self, url: &str, resume: Option<Resume>) -> Result<Response> {
        let mut request = self.client.get(url).header(ACCEPT_ENCODING, "identity");
        if let Some(resume) = &resume {
            request = request.header(RANGE, format!("bytes={}-", resume.from));
            if let Some(validator) = &resume.validator {
                request = request.header(IF_RANGE, validator);
            }
        }
        let response = request.send().map_err(|e| {
            let why = if e.is_timeout() {
                stall_message(url, STALL)
            } else {
                e.to_string()
            };
            Error::new(ErrorKind::Download, format!("cannot fetch {url}: {why}")).with_source(e)
        })?;
        let status = response.status();
        let content_range = header(&response, CONTENT_RANGE).and_then(|value| parse_content_range(&value));
        let content_length = header(&response, CONTENT_LENGTH).and_then(|value| value.parse().ok());
        let validator = header(&response, ETAG).or_else(|| header(&response, LAST_MODIFIED));
        // A 416 carries no body worth reading, and a non-2xx body is never data.
        let body: Box<dyn Read + Send> = if status.is_success() && status != StatusCode::NO_CONTENT {
            Box::new(Body {
                inner: response,
                url: url.to_owned(),
            })
        } else {
            Box::new(std::io::empty())
        };
        Ok(Response {
            status: status.as_u16(),
            content_range,
            content_length,
            validator,
            body,
        })
    }
}
