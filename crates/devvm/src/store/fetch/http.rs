//! The real transport: blocking HTTP over rustls, bounded only by stalls.

use std::io::Read;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, HeaderMap, HeaderName, IF_RANGE, LAST_MODIFIED, RANGE,
};

use super::{ContentRange, Response, Resume, Transport};
use crate::error::text_with_sources;
use crate::lazy::Lazy;
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
                Error::new(
                    ErrorKind::Internal,
                    format!("cannot build the HTTP client: {}", text_with_sources(&e)),
                )
                .with_source(e)
            })?;
        Ok(HttpTransport { client, stall })
    }
}

/// The real HTTP transport, built on the first `get`. A failed build is a `Download` error naming the URL and is not
/// kept: the next `get` builds again.
#[derive(Default)]
pub struct LazyHttp(Lazy<HttpTransport>);

impl LazyHttp {
    /// A transport that has not built its client yet.
    pub fn new() -> LazyHttp {
        LazyHttp::default()
    }
}

impl Transport for LazyHttp {
    fn get(&self, url: &str, resume: Option<Resume>) -> Result<Response> {
        self.0
            .get(|| {
                HttpTransport::new().map_err(|e| e.with_context(ErrorKind::Download, format!("cannot fetch {url}")))
            })?
            .get(url, resume)
    }
}

/// The text of a stall, for a message that already names the URL.
pub(super) fn stall_message(stall: Duration) -> String {
    format!("no data for {}s", stall.as_secs())
}

/// The text of a connect that timed out. It names no duration: `hyper-util` gives each address of the host a share of
/// the connect bound, so the wait can be shorter than the stall.
const CONNECT_TIMEOUT_MESSAGE: &str = "the connection timed out";

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
    stall: Duration,
}

/// Whether `e` is the expiry of the bound `with_stall` sets, and not a timeout the operating system reported.
/// `reqwest::Error::is_timeout` is true for both.
fn is_stall(e: &reqwest::Error) -> bool {
    e.is_timeout() && !comes_from_below(e)
}

/// Whether the connection attempt timed out on a bound devvm set, which is not the whole stall.
pub(super) fn connect_timed_out(e: &(dyn std::error::Error + 'static)) -> bool {
    first_io_error(e).is_some_and(holds_elapsed)
}

/// Whether an `io::Error` the operating system or the network stack made is in the chain of `e`. The timeouts devvm
/// sets are `reqwest`'s own marker error and, for the connect, an `io::Error` of kind `TimedOut` that holds tokio's
/// `Elapsed`; every other `io::Error` is from below.
pub(super) fn comes_from_below(e: &(dyn std::error::Error + 'static)) -> bool {
    first_io_error(e).is_some_and(|io| !holds_elapsed(io))
}

fn first_io_error<'a>(e: &'a (dyn std::error::Error + 'static)) -> Option<&'a std::io::Error> {
    let mut next = Some(e);
    while let Some(layer) = next {
        if let Some(io) = layer.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        next = layer.source();
    }
    None
}

/// The shape of `hyper-util`'s connect timeout, as of 0.1.21 (`client/legacy/connect/http.rs`, `connect`): an
/// `io::Error` of kind `TimedOut` holding tokio's `Elapsed`. Re-check it when `hyper-util` is bumped.
fn holds_elapsed(io: &std::io::Error) -> bool {
    io.get_ref()
        .is_some_and(|inner| inner.is::<tokio::time::error::Elapsed>())
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf).map_err(|e| {
            let stalled = e
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
                .is_some_and(is_stall);
            if stalled {
                std::io::Error::new(std::io::ErrorKind::TimedOut, stall_message(self.stall))
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
            let e = e.without_url();
            let why = if connect_timed_out(&e) {
                CONNECT_TIMEOUT_MESSAGE.to_owned()
            } else if is_stall(&e) {
                stall_message(self.stall)
            } else {
                text_with_sources(&e)
            };
            Error::new(ErrorKind::Download, format!("cannot fetch {url}: {why}")).with_source(e)
        })?;
        let status = response.status();
        let head = read_head(response.headers());
        // A 416 carries no body worth reading, and a non-2xx body is never data.
        let body: Box<dyn Read + Send> = if status.is_success() && status != StatusCode::NO_CONTENT {
            Box::new(Body {
                inner: response,
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
