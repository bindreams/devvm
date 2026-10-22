//! devvm: disposable virtual machines for tests.

#[cfg(test)]
#[path = "lib_tests.rs"]
mod lib_tests;

#[cfg(test)]
fn main() {
    skuld::run_all();
}
