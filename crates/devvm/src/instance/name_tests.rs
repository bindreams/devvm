use crate::ErrorKind;
use crate::instance::name::InstanceName;

#[skuld::test]
fn valid_names_parse() {
    let long = format!("a{}b", "-".repeat(61));
    assert_eq!(long.len(), 63);
    for text in ["a", "a1", "a-b", "0", "run-1234abcd", long.as_str()] {
        assert_eq!(InstanceName::parse(text).unwrap().as_str(), text);
    }
}

#[skuld::test]
fn uppercase_is_lowercased() {
    assert_eq!(InstanceName::parse("Foo").unwrap().as_str(), "foo");
    assert_eq!(InstanceName::parse("A-B1").unwrap().as_str(), "a-b1");
}

#[skuld::test]
fn invalid_names_are_rejected() {
    let too_long = "a".repeat(64);
    let invalid = [
        "",
        "-a",
        "a-",
        "a_b",
        "a.b",
        "a b",
        "../x",
        "a/b",
        r"a\b",
        "a*",
        "con",
        "CON",
        "com1",
        "COM9",
        "lpt9",
        "nul",
        "aux",
        "prn",
        too_long.as_str(),
        "ж",
        "a\0b",
        "a\n",
        // KELVIN SIGN: `str::to_lowercase` would turn it into the valid name `k`.
        "\u{212A}",
        "a\u{212A}",
    ];
    for text in invalid {
        let error = InstanceName::parse(text).expect_err(text);
        assert_eq!(error.kind(), ErrorKind::InvalidName, "{text:?}");
    }
}

#[skuld::test]
fn com10_and_console_are_valid() {
    for text in ["com10", "console", "lpt0", "com", "nulx"] {
        assert!(InstanceName::parse(text).is_ok(), "{text}");
    }
}

#[skuld::test]
fn the_message_shows_the_text_escaped_so_it_cannot_forge_a_line() {
    let error = InstanceName::parse("a\nb").unwrap_err();
    assert!(!error.message().contains('\n'), "{}", error.message());
    assert!(error.message().contains(r"a\nb"), "{}", error.message());
}

#[skuld::test]
fn generated_names_parse_and_differ() {
    let first = InstanceName::generate();
    let second = InstanceName::generate();
    assert_ne!(first.as_str(), second.as_str());
    for name in [&first, &second] {
        assert!(name.as_str().starts_with("run-"));
        assert_eq!(name.as_str().len(), "run-".len() + 8);
        assert_eq!(InstanceName::parse(name.as_str()).unwrap().as_str(), name.as_str());
    }
}
