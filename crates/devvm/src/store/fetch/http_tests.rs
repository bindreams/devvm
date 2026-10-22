use std::time::Duration;

use super::ContentRange;
use super::http::{parse_content_range, stall_message};

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
