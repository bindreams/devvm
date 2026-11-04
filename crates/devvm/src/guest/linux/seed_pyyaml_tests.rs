//! The seed read by PyYAML, which is what cloud-init uses. The Rust YAML reader is YAML 1.2 and agrees with the writer
//! on things PyYAML (1.1) does not, so only PyYAML can pin what the guest sees.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

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

/// Most characters of the reader's text that a line of the report shows; the rest is counted.
const SHOWN: usize = 60;

/// Code points of each side that a difference shows, from the first one that differs.
const WINDOW: usize = 8;

/// A seed's two files, and what PyYAML must read back from them: each field under the name `seed_pyyaml.py` writes it
/// with. Serialized, a case is what the script reads.
#[derive(Serialize)]
struct Case {
    #[serde(skip)]
    label: String,
    meta_data: String,
    user_data: String,
    #[serde(skip)]
    expected: [(&'static str, String); 5],
}

/// An entry of `seed_pyyaml.py`'s output; the contract is in its docstring.
#[derive(Deserialize)]
#[serde(untagged)]
enum Entry {
    Error { error: String },
    Fields(HashMap<String, Reading>),
}

/// What PyYAML read in one field.
#[derive(Deserialize)]
#[serde(untagged)]
enum Reading {
    Text(Vec<u32>),
    Other {
        #[serde(rename = "type")]
        kind: String,
        repr: String,
    },
}

/// The reader did not run to completion: not a finding about the seed.
fn reader_failed(why: String) -> ! {
    panic!(
        "{why}\nIf uv or PyYAML cannot be had on this host, {}.",
        pyyaml::OPT_OUT
    )
}

/// `text`, or its first [`SHOWN`] characters and its length when it is longer.
fn shortened(text: &str) -> String {
    let length = text.chars().count();
    if length <= SHOWN {
        return text.to_owned();
    }
    format!(
        "{}…(the text is {length} characters long)",
        text.chars().take(SHOWN).collect::<String>()
    )
}

/// What PyYAML read in `got` that differs from `case.expected`, one line per field.
fn differences(case: &Case, got: &Entry) -> Vec<String> {
    let fields = match got {
        Entry::Error { error } => return vec![format!("{}: does not read back: {}", case.label, shortened(error))],
        Entry::Fields(fields) => fields,
    };
    case.expected
        .iter()
        .filter_map(|(field, want)| {
            let want: Vec<u32> = want.chars().map(u32::from).collect();
            let difference = match fields.get(*field) {
                Some(Reading::Text(read)) if *read == want => return None,
                Some(Reading::Text(read)) => code_point_difference(read, &want),
                Some(Reading::Other { kind, repr }) => format!("reads back as {kind} {}", shortened(repr)),
                None => "is missing from the reader's output".to_owned(),
            };
            Some(format!("{}: {field} {difference}", case.label))
        })
        .collect()
}

/// Where `read` first differs from `want`: the index, both lengths and a window of each side. Code points outside
/// printable ASCII are shown as `U+XXXX`, space-separated from their neighbours.
fn code_point_difference(read: &[u32], want: &[u32]) -> String {
    let at = read
        .iter()
        .zip(want)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| read.len().min(want.len()));
    let window = |points: &[u32]| {
        points
            .iter()
            .skip(at)
            .take(WINDOW)
            .map(|&point| match char::from_u32(point) {
                Some(c) if (' '..='~').contains(&c) => c.to_string(),
                _ => format!("U+{point:04X}"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!(
        "differs at code point {at} (read {} code points, expected {}): [{}] for [{}]",
        read.len(),
        want.len(),
        window(read),
        window(want)
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
    let read: Vec<Entry> = std::fs::read(&read_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
        .unwrap_or_else(|e| {
            reader_failed(format!(
                "the PyYAML reader's output {} is unusable: {e}",
                read_path.display()
            ))
        });
    assert_eq!(read.len(), cases.len(), "the PyYAML reader wrote one entry per case");

    let report: Vec<String> = cases
        .iter()
        .zip(&read)
        .flat_map(|(case, got)| differences(case, got))
        .collect();
    assert!(
        report.is_empty(),
        "PyYAML does not read the seed back exactly:\n{}",
        report.join("\n"),
    );
}
