//! The seed read by PyYAML, which is what cloud-init uses. The Rust YAML reader is YAML 1.2 and agrees with the writer
//! on things PyYAML (1.1) does not, so only PyYAML can pin what the guest sees.

use std::path::Path;
use std::process::Command;

use serde_json::json;

use super::seed_files;
use crate::guest::{SeedKeys, SeedSpec};
use crate::test_support::fixtures::unicode_home;

/// The variable a CI job sets to claim that its runs cover the PyYAML reading of the seed.
const COVERS_PYYAML: &str = "DEVVM_COVERS_PYYAML";

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

/// Runs the test only where a job claims it. Without a claim the test is ignored and the reason says where it runs, so a
/// local run never reads as covering PyYAML. Where claimed, a missing `uv` or PyYAML fails the test.
fn pyyaml_claimed() -> Result<(), String> {
    match std::env::var(COVERS_PYYAML) {
        Ok(claimed) if !claimed.is_empty() => Ok(()),
        _ => Err("no PyYAML claimed; covered by CI (Test job on ubuntu-latest, which installs uv)".to_owned()),
    }
}

#[skuld::test(requires = [pyyaml_claimed])]
fn every_unicode_scalar_value_reads_back_through_pyyaml_in_every_key_and_the_hostname(
    #[fixture(unicode_home)] home: &Path,
) {
    let all: Vec<char> = (0..=0x10_FFFF).filter_map(char::from_u32).collect();
    let cases: Vec<serde_json::Value> = all
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
                    host_private: text.clone(),
                    host_public: text,
                },
            };
            let files = seed_files(&spec).unwrap();
            let file = |name: &str| {
                let file = files.iter().find(|file| file.name == name).unwrap();
                String::from_utf8(file.bytes.clone()).unwrap()
            };
            json!({
                "where": format!("U+{:04X}..U+{:04X} {context}", chunk[0] as u32, chunk[chunk.len() - 1] as u32),
                "meta_data": file("meta-data"),
                "user_data": file("user-data"),
                "hostname": spec.hostname,
                "client_public": spec.keys.client_public,
                "host_private": spec.keys.host_private,
                "host_public": spec.keys.host_public,
            })
        })
        .collect();
    let cases_path = home.join("cases.json");
    std::fs::write(&cases_path, serde_json::to_vec(&cases).unwrap()).unwrap();

    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/guest/linux/seed_pyyaml.py");
    let output = Command::new("uv")
        .args(["run", "--script"])
        .arg(&script)
        .arg(&cases_path)
        .output()
        .expect("`uv` must be installed to read the seed with PyYAML; CI installs it");
    assert!(
        output.status.success(),
        "PyYAML does not read the seed back exactly ({}):\n{}{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
