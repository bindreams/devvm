//! The Unicode test directory name, repeated from the library's `test_support::fixtures::UNICODE_DIR` (the CLI tests
//! cannot reach the library's test support).

/// Each character is a case: `\u{e9}` is in code page 1252, `\u{416}` is not (1251 has it), `\u{4e16}` is in no
/// single-byte code page, `\u{1f600}` is a UTF-16 surrogate pair, and the spaces split arguments.
pub const UNICODE_DIR: &str = "unicode-\u{e9} \u{416} \u{4e16} \u{1f600}";
