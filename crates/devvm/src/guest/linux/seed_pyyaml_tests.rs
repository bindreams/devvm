//! The seed read by PyYAML, which is what cloud-init uses. The Rust YAML reader is YAML 1.2 and agrees with the writer
//! on things PyYAML (1.1) does not, so only PyYAML can pin what the guest sees.

use std::path::Path;
use std::process::Command;

use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;

use super::seed_files;
use crate::guest::{SeedKeys, SeedSpec};
use crate::test_groups::{Group, pyyaml};
use crate::test_support::fixtures::unicode_home;

/// Scalar values per case.
const CHUNK: usize = 4096;

type Context = (&'static str, fn(char) -> String);

/// How each scalar value is surrounded in a field. PyYAML drops spaces around a line break and reads `---` after one as
/// a document separator, so a value must be checked in these contexts as well as bare.
const CONTEXTS: [Context; 3] = [
    ("bare", |c| c.to_string()),
    ("between spaces", |c| format!(" {c} ")),
    ("before ---", |c| format!("{c}--- ")),
];

/// A seed's two files and what PyYAML must read back from them, each field under the name `seed_pyyaml.py` reads it
/// with. Serialized, a case is what the script reads.
#[derive(Serialize)]
struct Case {
    label: String,
    meta_data: String,
    user_data: String,
    expected: [(&'static str, String); 5],
}

/// The reader did not run to completion: not a finding about the seed.
fn reader_failed(why: String) -> ! {
    panic!(
        "{why}\nIf uv or PyYAML cannot be had on this host, {}.",
        pyyaml::OPT_OUT
    )
}

/// A host without `uv` or PyYAML fails the test.
#[skuld::test]
fn every_unicode_scalar_value_reads_back_through_pyyaml_in_every_key_and_the_hostname(
    #[fixture(unicode_home)] home: &Path,
    #[fixture(pyyaml)] _group: &Group,
) {
    let all: Vec<char> = (0..=0x10_FFFF).filter_map(char::from_u32).collect();
    let cases: Vec<Case> = all
        .chunks(CHUNK)
        .flat_map(|chunk| CONTEXTS.map(|(context, unit)| (chunk, context, unit)))
        .map(|(chunk, context, unit)| {
            let text: String = chunk.iter().copied().map(unit).collect();
            let spec = SeedSpec {
                hostname: format!("h{text}"),
                dir: home.to_path_buf(),
                instance_id: hex::encode([1u8; 16]).parse().unwrap(),
                keys: SeedKeys {
                    client_public: format!("ssh-ed25519 {text} x"),
                    host_private: SecretString::from(text.clone()),
                    host_public: text,
                },
            };
            let files = seed_files(&spec).unwrap();
            let file = |name: &str| {
                let file = files.iter().find(|file| file.name == name).unwrap();
                String::from_utf8(file.bytes.clone()).unwrap()
            };
            Case {
                label: format!(
                    "U+{:04X}..U+{:04X} {context}",
                    chunk[0] as u32,
                    chunk[chunk.len() - 1] as u32
                ),
                meta_data: file("meta-data"),
                user_data: file("user-data"),
                expected: [
                    ("meta hostname", spec.hostname.clone()),
                    ("user hostname", spec.hostname),
                    ("client_public", spec.keys.client_public),
                    ("host_private", spec.keys.host_private.expose_secret().to_owned()),
                    ("host_public", spec.keys.host_public),
                ],
            }
        })
        .collect();
    let cases_path = home.join("cases.json");
    let read_path = home.join("read.json");
    std::fs::write(&cases_path, serde_json::to_vec(&cases).unwrap()).unwrap();

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/guest/linux/seed_pyyaml.py");
    let output = Command::new("uv")
        .args(["run", "--script"])
        .arg(&script)
        .arg(&cases_path)
        .arg(&read_path)
        .output()
        .unwrap_or_else(|e| reader_failed(format!("`uv` must be installed to read the seed with PyYAML ({e})")));
    if !output.status.success() {
        reader_failed(format!(
            "the PyYAML reader failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        ));
    }
    let report: Vec<String> = std::fs::read(&read_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| {
            reader_failed(format!(
                "the PyYAML reader's output {} is unusable: {e}",
                read_path.display()
            ))
        });
    assert!(
        report.is_empty(),
        "PyYAML does not read the seed back exactly:\n{}",
        report.join("\n"),
    );
}
