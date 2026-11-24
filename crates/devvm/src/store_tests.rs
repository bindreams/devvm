use std::time::{Duration, SystemTime};

use crate::store::time_parse;

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
