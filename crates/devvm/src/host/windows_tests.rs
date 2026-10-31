use std::path::Path;

use super::WindowsHost;
use crate::host::Host;
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn a_junction_reads_as_the_path_it_was_made_for_not_its_nt_name(#[fixture(unicode_home)] home: &Path) {
    let target = home.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = home.join("junction");
    junction::create(&target, &link).unwrap();
    let text = WindowsHost.read_link(&link).unwrap();
    assert!(!text.contains("??"), "{text}");
    assert_eq!(text, target.display().to_string().replace('\\', "/"));
}
