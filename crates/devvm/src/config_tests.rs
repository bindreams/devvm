use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::test_support::fixtures::anna_home;
use crate::{Dirs, ErrorKind};

fn origin() -> &'static Path {
    Path::new("cfg/config.yaml")
}

#[skuld::test]
fn missing_file_is_the_default_config(#[fixture(anna_home)] home: &Path) {
    let config = Config::load(&Dirs::under(home)).unwrap();
    assert_eq!(config, Config::default());
}

#[skuld::test]
fn load_reads_config_yaml_from_the_config_dir(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    std::fs::create_dir_all(&dirs.config).unwrap();
    let text = format!("qemu:\n  path: {}\n", r"C:\Program Files\qemu\qemu.exe");
    std::fs::write(dirs.config.join("config.yaml"), text).unwrap();
    let config = Config::load(&dirs).unwrap();
    assert_eq!(config.qemu_path, Some(PathBuf::from(r"C:\Program Files\qemu\qemu.exe")));
}

#[skuld::test]
fn qemu_path_is_read() {
    let config = Config::parse("qemu: { path: /opt/q/qemu-system }", origin()).unwrap();
    assert_eq!(config.qemu_path, Some(PathBuf::from("/opt/q/qemu-system")));
}

#[skuld::test]
fn empty_and_comment_only_files_are_the_default_config() {
    assert_eq!(Config::parse("", origin()).unwrap(), Config::default());
    assert_eq!(Config::parse("# nothing yet\n", origin()).unwrap(), Config::default());
    assert_eq!(Config::parse("qemu: {}", origin()).unwrap(), Config::default());
}

#[skuld::test]
fn unknown_key_is_an_error_naming_it_and_the_file() {
    for text in ["qemuu: 1", "qemu: { pth: x }"] {
        let error = Config::parse(text, origin()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
        assert!(error.message().contains("cfg/config.yaml"), "{error}");
        assert!(
            error.message().contains("qemuu") || error.message().contains("pth"),
            "{error}"
        );
    }
}

#[skuld::test]
fn invalid_yaml_error_names_the_file() {
    let error = Config::parse("qemu: [", origin()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(error.message().contains("cfg/config.yaml"), "{error}");
}

#[skuld::test]
fn bom_and_crlf_are_accepted() {
    let config = Config::parse("\u{feff}qemu:\r\n  path: /q\r\n", origin()).unwrap();
    assert_eq!(config.qemu_path, Some(PathBuf::from("/q")));
}

#[skuld::test]
fn unreadable_config_is_an_io_error_naming_the_path(#[fixture(anna_home)] home: &Path) {
    let dirs = Dirs::under(home);
    // A directory where the file should be.
    std::fs::create_dir_all(dirs.config.join("config.yaml")).unwrap();
    let error = Config::load(&dirs).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("config.yaml"), "{error}");
}
