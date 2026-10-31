use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256 as Sha2};

use super::{LinuxBuilder, definition_hash_of, probe_spec};
use crate::backend::fake::FakeStorage;
use crate::exit::NullProgress;
use crate::guest::GuestOs;
use crate::guest::linux::LinuxGuest;
use crate::guest::linux::seed::seed_files;
use crate::guest::{BaseBuilder, BuildCtx};
use crate::media::SeedFile;
use crate::recipe::{PerArch, Pin, Source};
use crate::store::fetch::{Fetcher, HttpTransport, Response, Resume, Transport};
use crate::store::layers::Built;
use crate::store::layers::layers_tests::{env, key, request};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::http::TestServer;
use crate::{Arch, Dirs, Error, ErrorKind, Sha256};

fn image(seed: u8) -> Vec<u8> {
    (0..4096usize).map(|i| (i as u8).wrapping_add(seed)).collect()
}

fn pin_for(server: &TestServer, data: &[u8]) -> Pin {
    Pin {
        url: server.url("/ubuntu.img"),
        sha256: Sha256::from_bytes(Sha2::digest(data).into()),
    }
}

fn builder(home: &Path) -> LinuxBuilder {
    let fetcher = Fetcher::new(
        Dirs::under(home).downloads(),
        Arc::new(HttpTransport::new().unwrap()),
        Arc::new(NullProgress),
    );
    LinuxBuilder::new(Arc::new(fetcher), Arc::new(FakeStorage))
}

fn ctx(home: &Path, arch: Arch) -> BuildCtx {
    BuildCtx {
        dirs: Dirs::under(home),
        arch,
        disk_size: 1 << 30,
        progress: Arc::new(NullProgress),
    }
}

/// The names in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[skuld::test]
fn build_base_places_disk_and_vars_in_staging_and_nowhere_else(#[fixture(unicode_home)] home: &Path) {
    let data = image(1);
    let server = TestServer::start(data.clone(), "\"v1\"");
    let source = Source::DiskImage(PerArch::any(pin_for(&server, &data)));
    let (builder, context) = (builder(home), ctx(home, Arch::Amd64));
    let env = env(home);

    let layer = env
        .store
        .build(&request(key(1)), None, |staging| {
            let before = names(staging.dir.path());
            assert!(before.is_empty(), "staging starts empty: {before:?}");
            let meta = builder.build_base(&source, &context, staging.dir)?;
            assert_eq!(meta.expires, None, "a pinned image does not expire");
            Ok(Built::from_meta(&meta))
        })
        .unwrap();

    let published = layer.tip().dir().path();
    assert_eq!(
        names(published),
        ["disk.qcow2", "meta.json"],
        "the fake storage writes only the disk"
    );
    assert_eq!(std::fs::read(published.join("disk.qcow2")).unwrap(), data);
    // The download is cached once, outside the layer.
    assert_eq!(server.requests().len(), 1);
}

#[skuld::test]
fn each_arch_fetches_only_its_own_pin(#[fixture(unicode_home)] home: &Path) {
    let (amd64, arm64) = (image(1), image(2));
    let (amd64_server, arm64_server) = (
        TestServer::start(amd64.clone(), "\"a\""),
        TestServer::start(arm64.clone(), "\"b\""),
    );
    let source = Source::DiskImage(PerArch {
        amd64: Some(pin_for(&amd64_server, &amd64)),
        arm64: Some(pin_for(&arm64_server, &arm64)),
    });
    let (builder, env) = (builder(home), env(home));

    for (arch, expected, own, other) in [
        (Arch::Arm64, &arm64, &arm64_server, &amd64_server),
        (Arch::Amd64, &amd64, &amd64_server, &arm64_server),
    ] {
        let (own_before, other_before) = (own.requests().len(), other.requests().len());
        let context = ctx(home, arch);
        let layer = env
            .store
            .build(&request(key(arch as u8 + 10)), None, |staging| {
                Ok(Built::from_meta(&builder.build_base(&source, &context, staging.dir)?))
            })
            .unwrap();
        let disk = layer.tip().dir().path().join("disk.qcow2");
        assert_eq!(&std::fs::read(disk).unwrap(), expected, "{arch:?}");
        assert_eq!(
            own.requests().len(),
            own_before + 1,
            "{arch:?}: its own image is fetched once"
        );
        assert_eq!(
            other.requests().len(),
            other_before,
            "{arch:?}: the other image is left alone"
        );
    }
}

/// Answers every request with a download error naming the URL, and remembers the URLs.
struct RefusingTransport(Mutex<Vec<String>>);

impl Transport for RefusingTransport {
    fn get(&self, url: &str, _resume: Option<Resume>) -> crate::Result<Response> {
        self.0.lock().unwrap().push(url.to_owned());
        Err(Error::new(
            ErrorKind::Download,
            format!("no network in this test: {url}"),
        ))
    }
}

#[skuld::test]
fn the_embedded_ubuntu_pin_for_the_other_arch_is_used_for_arch_arm64(#[fixture(unicode_home)] home: &Path) {
    let recipe = crate::recipe::embedded::get("ubuntu-24.04").expect("the embedded Ubuntu recipe");
    let Source::DiskImage(pins) = recipe.source else {
        panic!("the embedded Ubuntu recipe is a disk image");
    };
    let transport = Arc::new(RefusingTransport(Mutex::new(Vec::new())));
    let fetcher = Fetcher::new(Dirs::under(home).downloads(), transport.clone(), Arc::new(NullProgress));
    let builder = LinuxBuilder::new(Arc::new(fetcher), Arc::new(FakeStorage));
    let env = env(home);

    for arch in [Arch::Arm64, Arch::Amd64] {
        let context = ctx(home, arch);
        let source = Source::DiskImage(pins.clone());
        let error = env
            .store
            .build(&request(key(arch as u8 + 1)), None, |staging| {
                Ok(Built::from_meta(&builder.build_base(&source, &context, staging.dir)?))
            })
            .expect_err("the transport refuses");
        assert_eq!(error.kind(), ErrorKind::Download);
        assert!(
            error.message().contains(&pins.get(arch).unwrap().url),
            "{arch:?}: {}",
            error.message()
        );
    }
    let asked = transport.0.lock().unwrap().clone();
    assert_eq!(asked, [pins.arm64.unwrap().url, pins.amd64.unwrap().url]);
}

#[skuld::test]
fn a_recipe_with_no_pin_for_the_arch_names_it(#[fixture(unicode_home)] home: &Path) {
    let data = image(1);
    let server = TestServer::start(data.clone(), "\"v1\"");
    let source = Source::DiskImage(PerArch {
        amd64: Some(pin_for(&server, &data)),
        arm64: None,
    });
    let (builder, env, context) = (builder(home), env(home), ctx(home, Arch::Arm64));

    let error = env
        .store
        .build(&request(key(1)), None, |staging| {
            Ok(Built::from_meta(&builder.build_base(&source, &context, staging.dir)?))
        })
        .expect_err("no arm64 pin");
    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert!(error.message().contains("arm64"), "{}", error.message());
    assert!(server.requests().is_empty());
}

#[skuld::test]
fn a_non_disk_source_is_not_supported_naming_it(#[fixture(unicode_home)] home: &Path) {
    let (builder, env, context) = (builder(home), env(home), ctx(home, Arch::Amd64));
    let sources = [
        (
            Source::Iso {
                path: PathBuf::from("win11.iso"),
            },
            "ISO",
        ),
        (
            Source::Oci {
                reference: "ghcr.io/example/image".to_owned(),
                digest: "sha256:abc".to_owned(),
            },
            "ghcr.io/example/image",
        ),
        (Source::Embedded("ubuntu-24.04".to_owned()), "ubuntu-24.04"),
        (Source::Recipe(PathBuf::from("base.devvm.yaml")), "base.devvm.yaml"),
    ];
    for (n, (source, named)) in sources.iter().enumerate() {
        let error = env
            .store
            .build(&request(key(n as u8 + 1)), None, |staging| {
                Ok(Built::from_meta(&builder.build_base(source, &context, staging.dir)?))
            })
            .err()
            .unwrap_or_else(|| panic!("{source:?} is not a disk image"));
        assert_eq!(error.kind(), ErrorKind::NotSupported, "{source:?}");
        assert!(error.message().contains(named), "{source:?}: {}", error.message());
    }
}

#[skuld::test]
fn a_checksum_mismatch_is_a_checksum_error_and_publishes_no_layer(#[fixture(unicode_home)] home: &Path) {
    let data = image(1);
    let server = TestServer::start(image(9), "\"v1\"");
    let source = Source::DiskImage(PerArch::any(pin_for(&server, &data)));
    let (builder, env, context) = (builder(home), env(home), ctx(home, Arch::Amd64));

    let error = env
        .store
        .build(&request(key(1)), None, |staging| {
            Ok(Built::from_meta(&builder.build_base(&source, &context, staging.dir)?))
        })
        .expect_err("the image does not match its pin");
    assert_eq!(error.kind(), ErrorKind::Checksum);
    assert!(env.store.lookup(&key(1)).unwrap().is_none(), "nothing was published");
}

// The definition hash =================================================================================================

fn files(user_data: &str) -> Vec<SeedFile> {
    vec![
        SeedFile {
            name: "meta-data".to_owned(),
            bytes: b"meta".to_vec(),
        },
        SeedFile {
            name: "user-data".to_owned(),
            bytes: user_data.as_bytes().to_vec(),
        },
    ]
}

#[skuld::test]
fn definition_hash_changes_when_the_seed_template_changes() {
    let base = definition_hash_of(&files("template one"), 1);
    assert_eq!(base, definition_hash_of(&files("template one"), 1), "a pure function");
    assert_ne!(
        base,
        definition_hash_of(&files("template two"), 1),
        "the template is part of the hash"
    );
    assert_ne!(
        base,
        definition_hash_of(&files("template one"), 2),
        "so is the semantics version"
    );

    // Moving bytes between the name and the contents, or between two files, must not collide.
    let a = vec![SeedFile {
        name: "ab".to_owned(),
        bytes: b"c".to_vec(),
    }];
    let b = vec![SeedFile {
        name: "a".to_owned(),
        bytes: b"bc".to_vec(),
    }];
    assert_ne!(definition_hash_of(&a, 1), definition_hash_of(&b, 1));
}

#[skuld::test]
fn the_builders_definition_hash_is_the_hash_of_the_real_seed_and_follows_its_inputs(
    #[fixture(unicode_home)] home: &Path,
) {
    let builder = builder(home);
    let hash = builder.definition_hash();
    assert_eq!(hash, builder.definition_hash(), "stable");

    // The builder hashes `seed_files` of its probe together with the guest's semantics version, nothing less.
    let probe = probe_spec();
    let version = LinuxGuest.semantics_version();
    assert_eq!(hash, definition_hash_of(&seed_files(&probe).unwrap(), version));

    // A change to a real seed input, or to the semantics version, changes the hash.
    let mut other_hostname = probe_spec();
    other_hostname.hostname.push('x');
    let mut other_key = probe_spec();
    other_key.keys.client_public.push('x');
    for changed in [other_hostname, other_key] {
        assert_ne!(hash, definition_hash_of(&seed_files(&changed).unwrap(), version));
    }
    assert_ne!(hash, definition_hash_of(&seed_files(&probe).unwrap(), version + 1));
}
