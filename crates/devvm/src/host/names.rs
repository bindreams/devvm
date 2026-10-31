//! Which file names a host's file system can hold. The rules are plain functions so the tests can apply a Windows
//! host's rules on any OS.

#[cfg(test)]
#[path = "names_tests.rs"]
mod names_tests;

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

/// The longest a file name can be: in bytes of UTF-8 on Unix, in UTF-16 units on NTFS.
const NAME_MAX: usize = 255;

/// The rules of a Unix file system: any bytes except `/` and NUL, at most 255 of them.
#[cfg_attr(windows, allow(dead_code, reason = "used by the host this binary does not run on"))]
pub(crate) fn posix_problem(name: &str) -> Option<String> {
    if let Some(problem) = common_problem(name) {
        return Some(problem);
    }
    if name.len() > NAME_MAX {
        return Some(format!(
            "is {} bytes, over the {NAME_MAX} bytes a file name can have",
            name.len()
        ));
    }
    None
}

/// The rules of NTFS and Win32: no `\ : < > " | ? *`, no characters below U+0020, at most 255 UTF-16 units. Trailing
/// dots and spaces, device names and names that differ only in case are not checked: `devvm cp` takes trees without
/// them.
pub(crate) fn windows_problem(name: &str) -> Option<String> {
    if let Some(problem) = common_problem(name) {
        return Some(problem);
    }
    let units = name.encode_utf16().count();
    if units > NAME_MAX {
        return Some(format!(
            "is {units} UTF-16 units, over the {NAME_MAX} UTF-16 units a Windows file name can have"
        ));
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
    None
}
