//! The file name a download is stored under, derived from its URL and safe on every OS.

use crate::host::names::is_device_name;

/// Longest name kept. With the 64-digit cache directory, a download's path is the downloads directory plus at most 166
/// characters.
const MAX_LEN: usize = 100;

const FALLBACK: &str = "artifact";

#[cfg(test)]
#[path = "name_tests.rs"]
mod name_tests;

/// The last path segment of `url`, percent-decoded and reduced to `[A-Za-z0-9._-]`.
///
/// Query and fragment never reach the name. The result is never empty, never starts with a dot, never ends with a
/// dot or space, and is never a Windows device name (judged by the part before the first dot, as Windows does).
pub fn file_name(url: &str) -> String {
    let segment = url::Url::parse(url)
        .ok()
        .and_then(|url| {
            url.path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_owned))
        })
        .unwrap_or_default();
    let decoded = percent_encoding::percent_decode_str(&segment)
        .decode_utf8_lossy()
        .into_owned();

    let filtered: String = decoded
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let capped: String = filtered.trim_start_matches('.').chars().take(MAX_LEN).collect();
    let name = capped.trim_end_matches(['.', ' ']);

    if name.is_empty() || is_device_name(name) || is_device_name(&decoded) {
        FALLBACK.to_owned()
    } else {
        name.to_owned()
    }
}
