use crate::recipe::embedded;

#[skuld::test]
fn embedded_names_list_contains_ubuntu_24_04() {
    assert!(embedded::names().contains(&"ubuntu-24.04"));
}
