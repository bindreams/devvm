//! Leftovers with the claim a launch would have made for them.

use std::path::{Path, PathBuf};

use crate::backend::FileClaim;
use crate::host::{self, Host};

/// Makes the claim file `path`, which nobody holds afterwards, and returns the claim a record keeps for it.
pub fn claim_file(host: &dyn Host, path: &Path) -> std::io::Result<FileClaim> {
    let hold = host.hold_new(path)?;
    let id = hold.id()?;
    drop(hold);
    Ok(FileClaim {
        path: path.to_path_buf(),
        id,
    })
}

/// `leftovers` plus the claim file of their launch, as a record keeps them: a hold file `<first leftover>.hold`,
/// made here, which nobody holds, and last in the list. Empty `leftovers` have no claim.
pub fn claimed(mut leftovers: Vec<PathBuf>) -> (Vec<PathBuf>, Option<FileClaim>) {
    let Some(first) = leftovers.first() else {
        return (leftovers, None);
    };
    let mut name = first.clone().into_os_string();
    name.push(".hold");
    let path = PathBuf::from(name);
    let claim = claim_file(host::current(), &path).expect("the claim file is new and has an identity");
    leftovers.push(path);
    (leftovers, Some(claim))
}
