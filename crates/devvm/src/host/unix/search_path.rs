//! `O_PATH`: a directory opened to be searched, not read, as a base for `openat`.

use rustix::fs::OFlags;

pub(super) const SEARCH_ONLY: OFlags = OFlags::PATH;
