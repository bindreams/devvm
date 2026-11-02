use std::path::Path;

use super::sweep_tests::sweeping_a_link_touches_nothing_and_warns;
use crate::test_support::fixtures::unicode_home;

#[skuld::test]
fn sweeping_a_run_dir_that_is_a_junction_touches_nothing_and_warns(#[fixture(unicode_home)] home: &Path) {
    sweeping_a_link_touches_nothing_and_warns(home, |target, link| junction::create(target, link).unwrap());
}
