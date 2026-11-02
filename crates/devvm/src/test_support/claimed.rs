//! Leftovers with the claim a launch would have made for them.

use std::path::PathBuf;

use crate::backend::FileClaim;
use crate::host;

/// `leftovers` plus the claim file of their launch, as a record keeps them: a hold file `<first leftover>.hold`,
/// made here, which nobody holds, and last in the list. Empty `leftovers` have no claim.
pub fn claimed(mut leftovers: Vec<PathBuf>) -> (Vec<PathBuf>, Option<FileClaim>) {
    let Some(first) = leftovers.first() else {
        return (leftovers, None);
    };
    let mut name = first.clone().into_os_string();
    name.push(".hold");
    let path = PathBuf::from(name);
    let hold = host::current().hold_new(&path).expect("the claim file is new");
    let id = hold.id().expect("the claim file has an identity");
    drop(hold);
    leftovers.push(path.clone());
    (leftovers, Some(FileClaim { path, id }))
}
