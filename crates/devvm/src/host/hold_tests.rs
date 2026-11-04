use std::path::Path;

use super::{claim_of_deleting_name, deleting_name};

/// The sweep reads back what a delete wrote.
#[skuld::test]
fn a_deleting_name_gives_back_its_claim_file() {
    let name = deleting_name(
        Path::new("x.sock.hold"),
        [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
    );

    assert_eq!(name, "x.sock.hold.0123456789abcdef.del");
    assert_eq!(claim_of_deleting_name(name.to_str().unwrap()), Some("x.sock.hold"));
}
