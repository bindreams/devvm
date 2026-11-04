use std::io::{Error, ErrorKind};
use std::path::Path;

use super::link_error;

/// The host says the error is its file system lacking hard links (`FileModes::lacks_hard_links`).
#[skuld::test]
fn a_link_the_host_says_the_file_system_lacks_names_the_directory_hard_links_and_xdg_runtime_dir() {
    let dir = Path::new("some").join("run");
    for kind in [ErrorKind::Unsupported, ErrorKind::PermissionDenied] {
        let error = link_error(Error::from(kind), &dir.join("0000000000000000.sock.hold"), true);

        assert_eq!(error.kind(), kind);
        let text = error.to_string();
        assert!(text.contains(&dir.display().to_string()), "{text}");
        assert!(text.contains("hard links"), "{text}");
        assert!(text.contains("XDG_RUNTIME_DIR"), "{text}");
    }
}

#[skuld::test]
fn an_error_the_host_does_not_blame_on_hard_links_is_returned_as_it_was() {
    for kind in [
        ErrorKind::AlreadyExists,
        ErrorKind::StorageFull,
        ErrorKind::NotFound,
        ErrorKind::PermissionDenied,
        ErrorKind::Unsupported,
    ] {
        let error = link_error(
            Error::new(kind, "as it was"),
            Path::new("run").join("x.sock.hold").as_path(),
            false,
        );

        assert_eq!(error.kind(), kind);
        assert_eq!(error.to_string(), "as it was");
    }
}
