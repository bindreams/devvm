use std::io::{Error, ErrorKind};
use std::path::Path;

use super::link_error;

/// What `link(2)` returns on a file system without hard links: `EPERM` (`PermissionDenied`) on Linux, `ENOTSUP` or
/// `EOPNOTSUPP` (`Unsupported`) elsewhere.
#[skuld::test]
fn a_link_the_file_system_refuses_names_the_directory_hard_links_and_xdg_runtime_dir() {
    let dir = Path::new("some").join("run");
    for kind in [ErrorKind::Unsupported, ErrorKind::PermissionDenied] {
        let error = link_error(Error::from(kind), &dir.join("0000000000000000.sock.hold"));

        assert_eq!(error.kind(), kind);
        let text = error.to_string();
        assert!(text.contains(&dir.display().to_string()), "{text}");
        assert!(text.contains("hard links"), "{text}");
        assert!(text.contains("XDG_RUNTIME_DIR"), "{text}");
    }
}

#[skuld::test]
fn a_name_that_exists_and_any_other_failure_are_not_blamed_on_hard_links() {
    for kind in [ErrorKind::AlreadyExists, ErrorKind::StorageFull, ErrorKind::NotFound] {
        let error = link_error(
            Error::new(kind, "as it was"),
            Path::new("run").join("x.sock.hold").as_path(),
        );

        assert_eq!(error.kind(), kind);
        assert_eq!(error.to_string(), "as it was");
    }
}
