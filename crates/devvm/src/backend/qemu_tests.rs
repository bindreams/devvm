use std::path::Path;

use super::directory_of;
use crate::ErrorKind;

#[skuld::test]
fn a_path_without_a_directory_is_refused_naming_what_it_is() {
    // `Path::parent` gives an empty path for a bare file name, which would resolve against the working directory.
    for path in ["", "disk.img", ".", "/"] {
        let error = directory_of(Path::new(path), "the machine's disk").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Internal, "{path:?}: {error}");
        assert!(error.message().contains("the machine's disk"), "{path:?}: {error}");
    }
}

#[skuld::test]
fn a_path_in_a_directory_gives_the_directory() {
    let dir = std::env::temp_dir();
    assert_eq!(directory_of(&dir.join("disk.img"), "the disk").unwrap(), dir);
}
