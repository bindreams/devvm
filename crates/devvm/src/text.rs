//! Text from outside devvm, made safe to put in a message.

use std::borrow::Cow;
use std::fmt::Write;

#[cfg(test)]
#[path = "text_tests.rs"]
mod text_tests;

/// `text` with every control character and every [`reorders_or_splits`] character written as an escape (`\n`, `\r`,
/// `\t`, else `\u{..}`), so it cannot end or rewrite a line. Every other character is unchanged.
pub fn escape_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(needs_escape) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if needs_escape(c) => write!(escaped, "\\u{{{:x}}}", u32::from(c)).expect("writing to a String"),
            c => escaped.push(c),
        }
    }
    Cow::Owned(escaped)
}

fn needs_escape(c: char) -> bool {
    c.is_control() || reorders_or_splits(c)
}

/// Unicode `Bidi_Control` (reorders the text around it) and U+2028/U+2029 (read as line ends). Not all of Cf: the
/// zero-width joiner and soft hyphen are harmless.
fn reorders_or_splits(c: char) -> bool {
    matches!(
        c,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}'
    )
}
