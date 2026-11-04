//! The OS's random numbers.

#[cfg(test)]
#[path = "random_tests.rs"]
mod random_tests;

/// `N` bytes from the OS's random number generator.
pub(crate) fn bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the OS random number generator is available");
    bytes
}

/// Whether `text` is the lowercase hex of `bytes` bytes.
pub(crate) fn is_hex(text: &str, bytes: usize) -> bool {
    text.len() == 2 * bytes && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
