use std::path::Path;

use super::{claim_of_deleting_name, claim_of_new_name, deleting_name, new_name};

/// The sweep reads back what a delete wrote.
#[skuld::test]
fn a_deleting_name_gives_back_its_claim_file() {
    let name = deleting_name(
        Path::new("x.sock.hold"),
        [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
    );

    assert_eq!(name, Path::new("x.sock.hold.0123456789abcdef.del"));
    assert_eq!(claim_of_deleting_name(name.to_str().unwrap()), Some("x.sock.hold"));
}

/// The sweep reads back what a launch wrote.
#[skuld::test]
fn a_new_name_gives_back_its_claim_file() {
    let name = new_name(Path::new("x.sock.hold"));

    assert_eq!(name, Path::new("x.sock.hold.new"));
    assert_eq!(claim_of_new_name(name.to_str().unwrap()), Some("x.sock.hold"));
    assert_eq!(claim_of_new_name("x.sock.hold"), None);
}
