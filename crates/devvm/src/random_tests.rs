use super::{bytes, is_hex};

#[skuld::test]
fn is_hex_accepts_the_hex_of_that_many_bytes() {
    assert!(is_hex("0123456789abcdef", 8));
    assert!(is_hex("", 0));
    assert!(is_hex(&hex::encode(bytes::<8>()), 8));
}

#[skuld::test]
fn is_hex_refuses_any_other_text() {
    for (text, count) in [
        ("0123456789abcde", 8),
        ("0123456789abcdef0", 8),
        ("0123456789abcdeg", 8),
        ("0123456789ABCDEF", 8),
        ("0123456789abcde+", 8),
        ("+123456789abcdef", 8),
        ("0123456789abcdé", 8),
        ("0123456789abcdef", 7),
    ] {
        assert!(!is_hex(text, count), "{text} as {count} bytes");
    }
}
