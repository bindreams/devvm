//! Which file names a host's file system can hold, and when it calls two names one. The rules are plain functions so
//! the tests can apply a Windows host's rules on any OS.

#[cfg(test)]
#[path = "names_tests.rs"]
mod names_tests;

/// Device names Windows reserves, with or without an extension.
const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

/// What no host can hold as one file name.
fn common_problem(name: &str) -> Option<String> {
    if name.is_empty() || name == "." || name == ".." {
        return Some("is not a file name".to_owned());
    }
    if name.contains('/') || name.contains('\0') {
        return Some("contains a `/` or a NUL".to_owned());
    }
    None
}

/// The rules of a Unix file system: any bytes except `/` and NUL.
#[cfg_attr(windows, allow(dead_code, reason = "used by the host this binary does not run on"))]
pub(crate) fn posix_problem(name: &str) -> Option<String> {
    common_problem(name)
}

/// The rules of NTFS and Win32: no `\ : < > " | ? *`, no characters below U+0020, no trailing dot or space, no device names.
pub(crate) fn windows_problem(name: &str) -> Option<String> {
    if let Some(problem) = common_problem(name) {
        return Some(problem);
    }
    if let Some(c) = name
        .chars()
        .find(|c| matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Some(format!("contains `{c}`, which Windows file names cannot have"));
    }
    // NTFS holds DEL and the C1 controls.
    if name.chars().any(|c| ('\u{1}'..='\u{1f}').contains(&c)) {
        return Some("contains a control character, which Windows file names cannot have".to_owned());
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some("ends with a dot or a space, which Windows drops".to_owned());
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let digit = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|rest| matches!(rest.as_bytes(), [b'1'..=b'9']))
    };
    if RESERVED.contains(&stem.as_str()) || digit("COM") || digit("LPT") {
        return Some("is a device name, which Windows reserves".to_owned());
    }
    None
}

/// The key under which a case-insensitive file system (NTFS) files `name`: each character in simple upper case.
pub(crate) fn windows_key(name: &str) -> String {
    name.chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => single,
                _ => c,
            }
        })
        .collect()
}
