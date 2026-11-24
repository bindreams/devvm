use std::time::{Duration, SystemTime};

use crate::ErrorKind;
use crate::store::{time_parse, time_text};

#[skuld::test]
fn time_parse_reads_an_rfc3339_instant() {
    let parsed = time_parse("2027-01-15T08:00:00Z").unwrap();
    assert_eq!(parsed, SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000));
}

#[skuld::test]
fn time_parse_of_text_that_is_no_instant_names_the_text() {
    let error = time_parse("tomorrow").unwrap_err();
    assert!(error.starts_with("bad time `tomorrow`: "), "{error}");
}

#[skuld::test]
fn time_text_round_trips_through_time_parse() {
    let instant = SystemTime::UNIX_EPOCH + Duration::new(1_800_000_000, 123_456_789);
    assert_eq!(time_parse(&time_text(instant).unwrap()).unwrap(), instant);
}

#[skuld::test]
fn time_text_of_an_instant_past_year_9999_is_a_store_error() {
    // Year 11500: past jiff's last timestamp, and inside `SystemTime` on every platform.
    let instant = SystemTime::UNIX_EPOCH + Duration::from_secs(300_000_000_000);
    let error = time_text(instant).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.to_string().contains("time out of range"), "{error}");
}
