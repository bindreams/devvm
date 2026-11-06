use std::path::Path;
use std::sync::Arc;

use super::{directory_of, qemu_parts};
use crate::store::fetch::Fetcher;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::transport::RefusingTransport;
use crate::{Dirs, ErrorKind};

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

#[skuld::test]
fn the_parts_read_config_yaml_on_first_use_and_a_failed_read_is_not_kept(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let progress = Arc::new(MemoryProgress::default());
    let fetcher = Arc::new(Fetcher::new(
        dirs.downloads(),
        Arc::new(RefusingTransport::default()),
        progress.clone(),
    ));
    let parts = qemu_parts(&dirs, fetcher, &TestHost, progress);
    let file = dirs.config.join("config.yaml");

    // Written after the parts exist: a read at construction would have found no file.
    std::fs::create_dir_all(&dirs.config).unwrap();
    std::fs::write(&file, "qemu: [not, a, mapping]").unwrap();
    let error = parts.install.config().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config, "{error}");
    assert!(error.message().contains(&file.display().to_string()), "{error}");

    std::fs::write(
        &file,
        "qemu:
  path: tools
",
    )
    .unwrap();
    assert_eq!(
        parts.install.config().unwrap().qemu_path.as_deref(),
        Some(Path::new("tools"))
    );
}
