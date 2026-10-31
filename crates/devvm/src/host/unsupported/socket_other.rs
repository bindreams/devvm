//! The AF_UNIX path limit where there is no Unix socket address: only the Windows build of this module, used by tests.

pub(super) fn path_max() -> usize {
    107
}
