//! The OS's random numbers.

/// `N` bytes from the OS's random number generator.
pub(crate) fn bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the OS random number generator is available");
    bytes
}
