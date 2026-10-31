//! `O_SEARCH` (`O_EXEC | O_DIRECTORY` on Apple systems): a directory opened to be searched, not read. rustix has no
//! name for it.

use rustix::fs::OFlags;

pub(super) const SEARCH_ONLY: OFlags = OFlags::from_bits_retain(libc::O_SEARCH as u32);
