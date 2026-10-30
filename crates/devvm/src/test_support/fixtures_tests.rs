use std::path::PathBuf;

use super::UNICODE_DIR;

/// The other copies of [`UNICODE_DIR`]: the CLI tests and the QEMU acceptance script cannot reach the library's test
/// support. Each is `(file relative to this crate, start of the line that defines it, how it escapes a character)`.
const COPIES: [(&str, &str, Escapes); 2] = [
    (
        "../devvm-cli/tests/support/unicode.rs",
        "pub const UNICODE_DIR",
        Escapes::Rust,
    ),
    ("../../qemu/accept.py", "UNICODE_DIR =", Escapes::Python),
];

#[derive(Clone, Copy)]
enum Escapes {
    /// `\u{XXXX}`
    Rust,
    /// `\uXXXX` and `\UXXXXXXXX`
    Python,
}

/// The value of the string literal on the line of `source` that starts with `definition`.
fn literal(source: &str, definition: &str, escapes: Escapes) -> Result<String, String> {
    let line = source
        .lines()
        .find(|line| line.starts_with(definition))
        .ok_or_else(|| format!("no line starts with `{definition}`"))?;
    let (_, rest) = line
        .split_once("= \"")
        .ok_or_else(|| format!("`{line}` has no `= \"`"))?;
    let body = rest
        .strip_suffix("\";")
        .or_else(|| rest.strip_suffix('"'))
        .ok_or_else(|| format!("`{line}` does not end its literal"))?;
    decode(body, escapes)
}

fn decode(body: &str, escapes: Escapes) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let hex: String = match (escapes, chars.next()) {
            (Escapes::Rust, Some('u')) => {
                if chars.next() != Some('{') {
                    return Err(format!("`\\u` is not followed by `{{` in `{body}`"));
                }
                chars.by_ref().take_while(|&h| h != '}').collect()
            }
            (Escapes::Python, Some('u')) => chars.by_ref().take(4).collect(),
            (Escapes::Python, Some('U')) => chars.by_ref().take(8).collect(),
            (_, other) => return Err(format!("unsupported escape `\\{}` in `{body}`", other.unwrap_or(' '))),
        };
        let code = u32::from_str_radix(&hex, 16).map_err(|e| format!("`{hex}` is not hex in `{body}`: {e}"))?;
        out.push(char::from_u32(code).ok_or_else(|| format!("U+{code:X} is not a character in `{body}`"))?);
    }
    Ok(out)
}

#[skuld::test]
fn every_copy_of_unicode_dir_equals_the_librarys() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let differing: Vec<String> = COPIES
        .iter()
        .filter_map(|&(file, definition, escapes)| {
            let path = crate_dir.join(file);
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {} to compare its UNICODE_DIR: {e}", path.display()));
            match literal(&source, definition, escapes) {
                Ok(value) if value == UNICODE_DIR => None,
                Ok(value) => Some(format!("{file}: {value:?}")),
                Err(e) => Some(format!("{file}: {e}")),
            }
        })
        .collect();
    assert!(
        differing.is_empty(),
        "these copies of UNICODE_DIR differ from the library's {UNICODE_DIR:?} (test_support/fixtures.rs):\n{}",
        differing.join("\n"),
    );
}
