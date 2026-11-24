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
fn every_character_that_reorders_or_splits_a_line_is_written_as_an_escape() {
    let set = [
        '\u{61c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}', '\u{202e}', '\u{2066}',
        '\u{2067}', '\u{2068}', '\u{2069}', '\u{2028}', '\u{2029}',
    ];
    for c in set {
        assert_eq!(
            escape_controls(&format!("a{c}b")),
            format!("a\\u{{{:x}}}b", u32::from(c))
        );
    }
}

/// The characters next to the ends of the set's ranges: a widened range escapes one of them.
#[skuld::test]
fn the_neighbours_of_that_set_stay_as_they_are() {
    for c in ['\u{200b}', '\u{200d}', '\u{2060}', '\u{2065}', '\u{206a}', '\u{202f}'] {
        let text = format!("a{c}b");
        assert_eq!(escape_controls(&text), text);
    }
}

#[skuld::test]
fn a_joiner_inside_an_emoji_sequence_and_a_soft_hyphen_stay_as_they_are() {
    for text in ["\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", "co\u{ad}operate"] {
        assert_eq!(escape_controls(text), text);
    }
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
