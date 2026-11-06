//! Claim 6: paths std itself rejects. Measures an embedded NUL and the empty path.

use crate::calls::{RmOrder, probe};
use std::io;
use std::path::{Path, PathBuf};

pub fn run(label: &str, dir: &Path) -> io::Result<()> {
    let nul = dir.join("a\0b");
    probe("6-special", &format!("{label}:nul-in-name"), nul.as_os_str().len(), &nul, RmOrder::FileFirst, false);
    probe("6-special", &format!("{label}:empty-path"), 0, &PathBuf::new(), RmOrder::FileFirst, false);
    Ok(())
}
