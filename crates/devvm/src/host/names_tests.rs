use crate::host::names::{posix_problem, windows_key, windows_problem};

#[skuld::test]
fn posix_names_are_anything_but_nothing_dots_slash_and_nul() {
    for name in [
        "a",
        "a b",
        "a\\b",
        "C:\\z",
        "con",
        "x.",
        "\u{416}",
        "line\nbreak",
        "...",
        "-dash",
    ] {
        assert_eq!(posix_problem(name), None, "{name:?}");
    }
    for name in ["", ".", "..", "a/b", "a\0b"] {
        assert!(posix_problem(name).is_some(), "{name:?}");
    }
}

#[skuld::test]
fn windows_refuses_what_is_not_one_file_name() {
    for name in [
        "a\\b",
        "..\\x",
        "C:\\z",
        "C:z",
        "a:b",
        "a<b",
        "a>b",
        "a\"b",
        "a|b",
        "a?b",
        "a*b",
        "x.",
        "x ",
        "con",
        "CON",
        "Aux.txt",
        "nul.tar.gz",
        "com1",
        "LPT9.x",
        "co\u{7}",
        "",
        ".",
        "..",
        "a/b",
    ] {
        assert!(windows_problem(name).is_some(), "{name:?} was accepted");
    }
    for name in [
        "a",
        "a b",
        "\u{416}",
        "it's",
        "$x",
        "%x",
        "[a]",
        "-dash",
        "com0",
        "com10",
        "console",
        "a.b",
        ".hidden",
        "\u{1f600}",
    ] {
        assert_eq!(windows_problem(name), None, "{name:?}");
    }
}

#[skuld::test]
fn windows_keys_ignore_case_and_nothing_else() {
    assert_eq!(windows_key("a"), windows_key("A"));
    assert_eq!(windows_key("\u{e9}x"), windows_key("\u{c9}X"));
    assert_eq!(windows_key("\u{416}"), windows_key("\u{436}"));
    assert_ne!(windows_key("a"), windows_key("b"));
    // `\u{df}` upper-cases to two letters; it stays itself rather than becoming `SS`.
    assert_ne!(windows_key("\u{df}"), windows_key("SS"));
}
