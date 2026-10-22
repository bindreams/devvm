#[skuld::test]
fn the_skuld_harness_runs() {
    let two = std::hint::black_box(2);
    assert_eq!(two + 2, 4);
}
