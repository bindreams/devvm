use std::path::Path;

use super::relative_path;

#[skuld::test]
fn a_parent_layer_is_reached_two_levels_up_from_a_staging_directory() {
    let layers = Path::new("layers");
    let staging = layers.join("k2").join(".staging-abc");
    let parent = layers.join("k1").join("g1").join("disk.qcow2");
    assert_eq!(
        relative_path(&staging, &parent).as_deref(),
        Some("../../k1/g1/disk.qcow2")
    );
}

#[skuld::test]
fn a_sibling_generation_of_the_same_key_is_one_level_up() {
    let layers = Path::new("layers");
    let staging = layers.join("k1").join(".staging-abc");
    let parent = layers.join("k1").join("g1").join("disk.qcow2");
    assert_eq!(relative_path(&staging, &parent).as_deref(), Some("../g1/disk.qcow2"));
}

#[skuld::test]
fn a_path_with_spaces_and_non_ascii_keeps_them_with_slashes() {
    let layers = Path::new("base")
        .join(crate::test_support::fixtures::UNICODE_DIR)
        .join("layers");
    let staging = layers.join("k2").join(".staging-abc");
    let parent = layers.join("k1").join("g 1").join("disk.qcow2");
    assert_eq!(
        relative_path(&staging, &parent).as_deref(),
        Some("../../k1/g 1/disk.qcow2")
    );
}

#[skuld::test]
fn paths_with_no_common_root_have_no_relative_form() {
    assert_eq!(relative_path(Path::new("a/b"), Path::new("c/d")), None);
}
