use super::{sftp_path, split_root, user_path};
use crate::ErrorKind;
use crate::access::paths::check_guest_path;

const HOME: &str = "/C:/Users/user";

#[skuld::test]
fn a_drive_path_becomes_the_servers_form() {
    for (typed, sent) in [
        ("C:/x", "/C:/x"),
        ("c:/x/y.txt", "/c:/x/y.txt"),
        ("C:/", "/C:/"),
        ("C:", "/C:/"),
        ("/C:/x", "/C:/x"),
        ("/x", "/x"),
        ("x/y", "x/y"),
        ("//server/share/x", "//server/share/x"),
    ] {
        assert_eq!(sftp_path(typed, HOME), sent, "{typed}");
    }
}

#[skuld::test]
fn a_tilde_path_is_expanded_by_devvm_from_the_home_it_read() {
    // The server does not expand `~`: `~/x` reaches a file named `~` in the home.
    assert_eq!(sftp_path("~", HOME), "/C:/Users/user");
    assert_eq!(sftp_path("~/x/y", HOME), "/C:/Users/user/x/y");
    assert_eq!(sftp_path("~/", HOME), "/C:/Users/user/");
}

#[skuld::test]
fn a_tilde_backslash_path_is_another_users_home() {
    for typed in ["~\\x", "~\\", "~user/x"] {
        let error = check_guest_path(typed).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{typed}");
    }
    let error = check_guest_path("~\\x\\y").unwrap_err();
    assert!(error.message().contains("write ~/x/y"), "{error}");
    check_guest_path("~/x").unwrap();
}

#[skuld::test]
fn a_windows_user_path_undoes_sftp_path() {
    for typed in ["C:/", "C:/x", "c:/x/y.txt", "x/y", "/x", "//server/share/x"] {
        assert_eq!(user_path(&sftp_path(typed, HOME)), typed, "{typed}");
    }
    // What the server reports is what the user would type.
    assert_eq!(user_path("/C:/Users/user"), "C:/Users/user");
    assert_eq!(user_path("/C:"), "C:");
}

#[skuld::test]
fn a_destination_typed_with_backslashes_suggests_the_slash_spelling() {
    for (typed, suggestion) in [
        ("C:\\x\\y", "C:/x/y"),
        ("\\x", "/x"),
        ("x\\y", "x/y"),
        ("\\\\server\\share\\x", "//server/share/x"),
    ] {
        let error = split_root(typed).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{typed}");
        assert!(error.message().contains(typed), "{typed}: {error}");
        assert!(
            error.message().contains(&format!("write {suggestion}")),
            "{typed}: {error}"
        );
    }
}

#[skuld::test]
fn the_roots_of_a_windows_path() {
    for (typed, root, rest) in [
        ("C:/x/y", "C:/", "x/y"),
        ("C:", "C:/", ""),
        ("/C:/x", "C:/", "x"),
        ("/C:", "C:/", ""),
        ("//server/share/x", "//server/share/", "x"),
        ("/x", "/", "x"),
        ("x", "", "x"),
    ] {
        let (found, remaining) = split_root(typed).unwrap();
        assert_eq!((found.as_str(), remaining), (root, rest), "{typed}");
    }
    // Relative to the current directory of a drive: no path names it.
    assert_eq!(split_root("C:a").unwrap_err().kind(), ErrorKind::Usage);
}
