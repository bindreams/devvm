use crate::ErrorKind;
use crate::access::paths::{check_guest_path, expand_guest_path};

#[skuld::test]
fn guest_path_cases() {
    let expanded = [
        ("~", "/home/user"),
        ("~/a b", "/home/user/a b"),
        ("~/", "/home/user/"),
        ("/abs", "/abs"),
        ("rel", "rel"),
        ("a~b", "a~b"),
        ("~root/x", "~root/x"),
    ];
    for (path, want) in expanded {
        assert_eq!(expand_guest_path(path, "/home/user"), want, "{path}");
    }
    // A home with a trailing slash does not double it.
    assert_eq!(expand_guest_path("~/x", "/home/user/"), "/home/user/x");
    for path in ["~", "~/a b", "~/", "/abs", "rel", "a~b", ""] {
        check_guest_path(path).expect(path);
    }
    for path in ["~root/x", "~root", "~."] {
        assert_eq!(
            check_guest_path(path).expect_err(path).kind(),
            ErrorKind::Usage,
            "{path}"
        );
    }
}
