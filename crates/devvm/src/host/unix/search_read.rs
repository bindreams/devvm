//! A Unix with neither `O_PATH` nor `O_SEARCH` in rustix or devvm's reach: a directory can be opened only to be read,
//! so one that can be searched but not read is refused.

use rustix::fs::OFlags;

pub(super) const SEARCH_ONLY: OFlags = OFlags::RDONLY;
