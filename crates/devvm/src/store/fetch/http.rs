//! The real transport: blocking HTTP over rustls, bounded only by stalls.

use std::io::Read;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, HeaderMap, HeaderName, IF_RANGE, LAST_MODIFIED, RANGE,
};

use super::{ContentRange, Response, Resume, Transport};
use crate::{Error, ErrorKind, Result};

/// A connection attempt, or a wait for the next bytes, that gets nothing for this long fails. There is no bound on
/// the whole download: a slow link that keeps delivering is fine.
const STALL: Duration = Duration::from_secs(60);

pub struct HttpTransport {
    client: reqwest::blocking::Client,
    stall: Duration,
}

impl HttpTransport {
    pub fn new() -> Result<HttpTransport> {
        HttpTransport::with_stall(STALL)
    }

    /// `new` with another stall bound.
    pub(crate) fn with_stall(stall: Duration) -> Result<HttpTransport> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(stall)
            // On the blocking client this bounds the wait for the response head and each body read, not the transfer.
            .timeout(stall)
            .user_agent(concat!("devvm/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| {
                Error::new(ErrorKind::Internal, format!("cannot build the HTTP client: {e}")).with_source(e)
            })?;
        Ok(HttpTransport { client, stall })
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
    stall: Duration,
}

/// Whether a body read failed for lack of data. `reqwest`'s blocking body wraps its timeout in an `io::Error` of kind
/// `Other`, so the kind alone does not say.
fn is_stall(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::TimedOut
        || e.get_ref()
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf).map_err(|e| {
            if is_stall(&e) {
                std::io::Error::new(std::io::ErrorKind::TimedOut, stall_message(&self.url, self.stall))
            } else {
                e
            }
        })
    }
}

/// `name`'s value through `parse`; a present value that does not parse is ignored and named in `warnings`.
fn parse_header<T>(
    headers: &HeaderMap,
    name: HeaderName,
    label: &str,
    parse: impl Fn(&str) -> Option<T>,
    warnings: &mut Vec<String>,
) -> Option<T> {
    let value = headers.get(&name)?;
    let text = String::from_utf8_lossy(value.as_bytes());
    let parsed = parse(&text);
    if parsed.is_none() {
        warnings.push(format!("ignoring the malformed {label} `{text}`"));
    }
    parsed
}

/// What the headers say about the body.
pub(super) struct Head {
    pub content_range: Option<ContentRange>,
    pub content_length: Option<u64>,
    pub validator: Option<String>,
    /// Headers that were present but unusable, one sentence each.
    pub warnings: Vec<String>,
}

/// Reads `Content-Range`, `Content-Length` and the validator. A header that is present but malformed is ignored and
/// named in `warnings`.
pub(super) fn read_head(headers: &HeaderMap) -> Head {
    let mut warnings = Vec::new();
    let content_range = parse_header(
        headers,
        CONTENT_RANGE,
        "Content-Range",
        parse_content_range,
        &mut warnings,
    );
    let content_length = parse_header(
        headers,
        CONTENT_LENGTH,
        "Content-Length",
        |text| text.trim().parse().ok(),
        &mut warnings,
    );
    let text = |name: HeaderName| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    // A weak ETag never matches in `If-Range` (RFC 9110 13.1.5), so it would make every resume restart.
    let validator = text(ETAG)
        .filter(|etag| !etag.starts_with("W/"))
        .or_else(|| text(LAST_MODIFIED));
    Head {
        content_range,
        content_length,
        validator,
        warnings,
    }
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
                stall_message(url, self.stall)
            } else {
                e.to_string()
            };
            Error::new(ErrorKind::Download, format!("cannot fetch {url}: {why}")).with_source(e)
        })?;
        let status = response.status();
        let head = read_head(response.headers());
        // A 416 carries no body worth reading, and a non-2xx body is never data.
        let body: Box<dyn Read + Send> = if status.is_success() && status != StatusCode::NO_CONTENT {
            Box::new(Body {
                inner: response,
                url: url.to_owned(),
                stall: self.stall,
            })
        } else {
            Box::new(std::io::empty())
        };
        Ok(Response {
            status: status.as_u16(),
            content_range: head.content_range,
            content_length: head.content_length,
            validator: head.validator,
            warnings: head.warnings,
            body,
        })
    }
}
