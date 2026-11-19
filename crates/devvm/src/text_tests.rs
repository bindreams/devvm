use super::escape_controls;

#[skuld::test]
fn every_control_character_is_written_as_an_escape() {
    assert_eq!(escape_controls("a\nb\rc\td"), r"a\nb\rc\td");
    assert_eq!(
        escape_controls("\u{0}\u{1b}\u{7f}\u{85}\u{9f}"),
        r"\u{0}\u{1b}\u{7f}\u{85}\u{9f}"
    );
}

#[skuld::test]
fn every_other_character_stays_as_it_is() {
    for text in [
        "",
        "Windows 11 Enterprise",
        "say \"hi\" `now` 'x' \\ back",
        "e\u{301} \u{43f}\u{440}\u{43e}\u{431}\u{435}\u{43b} \u{1f600} \u{a0}\u{200b}",
    ] {
        assert_eq!(escape_controls(text), text);
    }
}
