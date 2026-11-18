//! Text from outside devvm, made safe to put in a message.

use std::borrow::Cow;
use std::fmt::Write;

#[cfg(test)]
#[path = "text_tests.rs"]
mod text_tests;

/// `text` with every control character (newline, carriage return, tab, the rest of C0 and C1, DEL) written as an escape
/// (`\n`, `\r`, `\t`, else `\u{..}`) so it cannot end or rewrite a line of a message. Every other character stays as it
/// is, so a quoted name reads as it is typed.
pub(crate) fn escape_controls(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut escaped = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if c.is_control() => write!(escaped, "\\u{{{:x}}}", u32::from(c)).expect("writing to a String"),
            c => escaped.push(c),
        }
    }
    Cow::Owned(escaped)
}
