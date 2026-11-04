//! A directory that belongs to another account, for the tests that must be refused one.

#[cfg(unix)]
#[path = "other_account/unix.rs"]
mod imp;
#[cfg(windows)]
#[path = "other_account/windows.rs"]
mod imp;

pub use imp::a_directory_of_another_account;
