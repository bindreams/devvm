use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256 as Sha2};

use super::qemu_check;
use crate::Arch;
use crate::Dirs;
use crate::Sha256;
use crate::backend::qemu::args::qemu_arch;
use crate::backend::qemu::sidecar::sidecar_tests::{make_firmware, make_tools};
use crate::backend::qemu::sidecar::{LazyInstall, ManifestEntry, Source, manifest_entry};
use crate::config::Config;
use crate::store::fetch::Fetcher;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::qemu::write_config;
use crate::test_support::transport::RefusingTransport;

const VERSION: &str = "11.1.2";

fn pinned() -> ManifestEntry {
    ManifestEntry {
        qemu_version: VERSION.to_owned(),
        url: "https://example.test/qemu.zip".to_owned(),
        sha256: Sha256::from_bytes(Sha2::digest(b"the pinned zip").into()),
    }
}

/// A `LazyInstall` over `dirs` and `entry`, and the transport it would download through.
fn install_over(dirs: &Dirs, entry: crate::Result<ManifestEntry>) -> (LazyInstall, Arc<RefusingTransport>) {
    let transport = Arc::new(RefusingTransport::default());
    let progress = Arc::new(MemoryProgress::default());
    let fetcher = Arc::new(Fetcher::new(dirs.downloads(), transport.clone(), progress.clone()));
    let verify = Arc::new(|_: &_, _: &str| panic!("the check never runs a QEMU tool"));
    let source = Source::new(dirs.clone(), fetcher, progress, entry, verify);
    (LazyInstall::from_source(source), transport)
}

fn configure(dirs: &Dirs, qemu_path: &Path) {
    write_config(
        dirs,
        &Config {
            qemu_path: Some(qemu_path.to_path_buf()),
        },
    );
}

#[skuld::test]
fn a_manifest_build_for_the_triple_passes_the_qemu_check_without_downloading(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let (install, transport) = install_over(&dirs, Ok(pinned()));

    let check = qemu_check(&install);

    assert_eq!(check.name, "qemu");
    let detail = check
        .outcome
        .expect("a fresh install passes before anything is downloaded");
    assert!(detail.contains(VERSION), "{detail}");
    assert!(detail.contains(&pinned().sha256.to_string()), "{detail}");
    assert!(detail.contains("downloaded on first use"), "{detail}");
    assert!(transport.asked().is_empty(), "{:?}", transport.asked());
}

#[skuld::test]
fn a_configured_qemu_path_that_is_accepted_passes_and_names_its_hash(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let bin = home.join("my qemu").join("bin");
    make_tools(&bin);
    make_firmware(&bin.join("share"));
    let system = bin.join(format!(
        "qemu-system-{}{}",
        qemu_arch(Arch::host()),
        std::env::consts::EXE_SUFFIX
    ));
    std::fs::write(&system, b"the configured qemu").unwrap();
    configure(&dirs, &bin);
    let (install, transport) = install_over(&dirs, Ok(pinned()));

    let detail = qemu_check(&install).outcome.expect("the configured path is accepted");

    assert!(detail.contains(&bin.display().to_string()), "{detail}");
    assert!(
        detail.contains(&Sha256::from_bytes(Sha2::digest(b"the configured qemu").into()).to_string()),
        "{detail}"
    );
    assert!(
        !detail.contains(VERSION),
        "a configured QEMU has no pinned version: {detail}"
    );
    assert!(transport.asked().is_empty());
}

#[skuld::test]
fn a_configured_path_that_is_not_accepted_fails_naming_the_path(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let empty = home.join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    configure(&dirs, &empty);
    let (install, _) = install_over(&dirs, Ok(pinned()));

    let why = qemu_check(&install).outcome.expect_err("the directory holds no QEMU");

    assert!(why.contains(&empty.display().to_string()), "{why}");
    assert!(why.contains("qemu-img"), "{why}");
}

#[skuld::test]
fn no_build_for_the_triple_and_no_path_fails_the_qemu_check_naming_the_triple(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let (install, transport) = install_over(&dirs, manifest_entry("riscv64-unknown-plan9"));

    let why = qemu_check(&install).outcome.expect_err("no build for this triple");

    assert!(why.contains("riscv64-unknown-plan9"), "{why}");
    assert!(transport.asked().is_empty());
}

#[skuld::test]
fn a_configuration_that_cannot_be_read_fails_the_qemu_check_naming_the_file(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    std::fs::create_dir_all(&dirs.config).unwrap();
    let file = dirs.config.join("config.yaml");
    std::fs::write(&file, "qemu: [not, a, mapping]").unwrap();
    let (install, _) = install_over(&dirs, Ok(pinned()));

    let why = qemu_check(&install)
        .outcome
        .expect_err("the configuration is unreadable");

    assert!(why.contains(&file.display().to_string()), "{why}");
}
