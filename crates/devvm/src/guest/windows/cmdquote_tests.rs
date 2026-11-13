use super::{batch_arguments, command_line_arguments};

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Each case is what std's `append_bat_arg` writes for the argument (read from the std source: quote unless every
/// character is in `[0-9A-Za-z#$*+-./:?@\_]` or non-ASCII; double a `"`; double a backslash run before a quote; write
/// `%` as `%%cd:~,%`).
#[skuld::test]
fn an_argument_is_quoted_the_way_std_quotes_it_for_a_batch_file() {
    for (given, want) in [
        ("abc", "abc"),
        (r"C:\x\y.txt", r"C:\x\y.txt"),
        ("a b", r#""a b""#),
        ("", r#""""#),
        ("a&b", r#""a&b""#),
        ("a|b", r#""a|b""#),
        ("^caret", r#""^caret""#),
        (r#"a"b"#, r#""a""b""#),
        (r#"a\"b"#, r#""a\\""b""#),
        (r"a\", r#""a\\""#),
        ("50%", r#""50%%cd:~,%""#),
        ("%PATH%", r#""%%cd:~,%PATH%%cd:~,%""#),
        ("a\tb", "\"a\tb\""),
        ("a\u{7f}b", "\"a\u{7f}b\""),
        ("é世", "é世"),
        ("--%", r#""--%%cd:~,%""#),
        ("a=b", r#""a=b""#),
    ] {
        assert_eq!(
            batch_arguments(&args(&[given])).unwrap(),
            format!(" {want}"),
            "{given:?}"
        );
    }
}

#[skuld::test]
fn arguments_are_joined_each_after_a_space() {
    assert_eq!(batch_arguments(&args(&["a b", "c"])).unwrap(), r#" "a b" c"#);
    assert_eq!(batch_arguments(&[]).unwrap(), "");
}

#[skuld::test]
fn a_batch_argument_std_refuses_is_refused_naming_its_place() {
    for (given, why) in [("a\nb", "line break"), ("a\rb", "line break"), ("a\0b", "NUL")] {
        let error = batch_arguments(&args(&["fine", given])).unwrap_err();
        assert!(error.starts_with("argument 2 "), "{given:?}: {error}");
        assert!(error.contains(why), "{given:?}: {error}");
    }
}

#[skuld::test]
fn a_cmd_command_is_its_switches_then_its_text_in_one_pair_of_quotes() {
    for (argv, want) in [
        (&["cmd", "/d", "/c", "echo hi"][..], r#"/d /c "echo hi""#),
        (&["cmd.exe", "/c", r#"echo "a" & echo b"#], r#"/c "echo "a" & echo b""#),
        (&["CMD", "/D", "/C", "dir"], r#"/D /C "dir""#),
        (&["cmd", "/s", "/d", "/c", "a", "b"], r#"/s /d /c "a b""#),
    ] {
        assert_eq!(command_line_arguments(&args(argv)).unwrap().unwrap(), want, "{argv:?}");
    }
}

#[skuld::test]
fn a_cmd_argv_that_is_not_a_command_has_no_command_line() {
    for argv in [
        &["cmd"][..],
        &["cmd", "/d"],
        &["cmd", "/c"],
        &["cmd", "/d", "/c"],
        &["cmd", "echo"],
    ] {
        assert!(command_line_arguments(&args(argv)).is_none(), "{argv:?}");
    }
}

#[skuld::test]
fn a_cmd_command_with_a_line_break_is_refused() {
    let error = command_line_arguments(&args(&["cmd", "/c", "echo a\necho b"]))
        .unwrap()
        .unwrap_err();
    assert!(error.contains("argument 2") && error.contains("line break"), "{error}");
}
