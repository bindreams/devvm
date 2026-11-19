//! Assertions about who can read a file devvm made private. Windows only: Unix checks the mode (`file_mode::mode`).

use std::path::Path;

use super::file_security::{Security, set_protected_dacl};

/// Makes `dir` hand `Authenticated Users` full access to everything created below it, as the root of a fresh NTFS
/// volume does. Returns the user's SID.
pub fn let_other_accounts_write_below(dir: &Path) -> String {
    use crate::host::windows::user::current_sid;
    const AUTHENTICATED_USERS: &str = "S-1-5-11";

    let me = current_sid().unwrap();
    set_protected_dacl(dir, &format!("D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;AU)"));
    let probe = dir.join("probe");
    std::fs::write(&probe, "x").unwrap();
    let inherited = Security::of(&probe).allowed_sids();
    assert!(
        inherited.iter().any(|sid| sid == AUTHENTICATED_USERS),
        "a new file in the directory must inherit Authenticated Users write: {inherited:?}"
    );
    std::fs::remove_file(&probe).unwrap();
    me
}

/// The owner may be Administrators as well as the user: OpenSSH accepts both.
pub fn assert_is_the_users_alone(path: &Path, me: &str) {
    let security = Security::of(path);
    assert!(
        security.dacl_is_protected(),
        "{}: the DACL must be protected",
        path.display()
    );
    assert_eq!(security.allowed_sids(), vec![me.to_owned()], "{}", path.display());
    let owner = security.owner();
    assert!(
        owner == me || owner == "S-1-5-32-544",
        "{}: the owner must be the user or Administrators, not {owner}",
        path.display()
    );
}
