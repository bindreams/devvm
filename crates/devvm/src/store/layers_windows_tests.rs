use crate::store::time_parse;

/// The earliest timestamp jiff holds is outside `SystemTime`'s range on Windows; on Linux and macOS every timestamp
/// fits, so this error cannot be reached there.
#[skuld::test]
fn time_parse_of_a_timestamp_out_of_system_times_range_names_the_text() {
    let text = jiff::Timestamp::MIN.to_string();
    let error = time_parse(&text).unwrap_err();
    assert!(error.starts_with(&format!("bad time `{text}`: ")), "{error}");
    assert!(error.contains("out of range"), "{error}");
}
