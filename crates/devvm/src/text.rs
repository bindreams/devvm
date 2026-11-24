//! Text from outside devvm, made safe to put in a message.

use std::borrow::Cow;
use std::fmt::Write;

#[cfg(test)]
#[path = "text_tests.rs"]
mod text_tests;

/// `text` with every control character (newline, carriage return, tab, the rest of C0 and C1, DEL) and every character
/// that reorders or splits a line ([`reorders_or_splits`]) written as an escape (`\n`, `\r`, `\t`, else `\u{..}`), so it
/// cannot end or rewrite a line of a message. Every other character stays as it is, so a quoted name reads as it is
/// typed.
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

/// The characters of Unicode's `Bidi_Control` property, which reorder the text around them, and the line and paragraph
/// separators, which a reader may take for a line end. Not the rest of the format category: a zero-width joiner in an
/// emoji sequence and the soft hyphen are harmless.
fn reorders_or_splits(c: char) -> bool {
    matches!(
        c,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{2028}' | '\u{2029}'
    )
}
