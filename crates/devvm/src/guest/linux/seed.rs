//! The NoCloud seed: the files cloud-init reads at first boot.

use serde::Serialize;

use crate::guest::SeedSpec;
use crate::media::SeedFile;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "seed_tests.rs"]
mod seed_tests;

/// The guest account every command runs as.
pub(super) const ACCOUNT: &str = "user";

#[derive(Serialize)]
struct MetaData<'a> {
    #[serde(rename = "instance-id")]
    instance_id: String,
    #[serde(rename = "local-hostname")]
    local_hostname: &'a str,
}

#[derive(Serialize)]
struct UserData<'a> {
    hostname: &'a str,
    ssh_pwauth: bool,
    users: [User<'a>; 1],
    ssh_keys: HostKeys<'a>,
}

#[derive(Serialize)]
struct User<'a> {
    name: &'a str,
    shell: &'a str,
    sudo: &'a str,
    ssh_authorized_keys: [&'a str; 1],
}

#[derive(Serialize)]
struct HostKeys<'a> {
    ed25519_private: &'a str,
    ed25519_public: &'a str,
}

/// `meta-data` and `user-data` for `spec`. A pure function of `spec`: cloud-init applies keys and host keys once per
/// instance id, so every start of one instance must present the same id and the same keys.
pub(super) fn seed_files(spec: &SeedSpec) -> Result<Vec<SeedFile>> {
    let meta = MetaData {
        instance_id: format!("devvm-{}", spec.instance_id),
        local_hostname: &spec.hostname,
    };
    let user = UserData {
        hostname: &spec.hostname,
        ssh_pwauth: false,
        users: [User {
            name: ACCOUNT,
            shell: "/bin/bash",
            sudo: "ALL=(ALL) NOPASSWD:ALL",
            ssh_authorized_keys: [&spec.keys.client_public],
        }],
        ssh_keys: HostKeys {
            ed25519_private: &spec.keys.host_private,
            ed25519_public: &spec.keys.host_public,
        },
    };
    Ok(vec![
        SeedFile {
            name: "meta-data".to_owned(),
            bytes: document(&meta)?.into_bytes(),
        },
        SeedFile {
            name: "user-data".to_owned(),
            bytes: format!("#cloud-config\n{}", document(&user)?).into_bytes(),
        },
    ])
}

/// `value` as a document cloud-init reads. JSON is YAML, and it quotes every string. cloud-init reads the seed with
/// PyYAML (YAML 1.1), where an unquoted `yes`, `off` or `2024-01-15` is a boolean or a date, and a YAML writer
/// decides what to quote by the 1.2 rules.
///
/// `serde_json` escapes only U+0000 to U+001F, `"` and `\`. PyYAML rejects U+007F, U+0080 to U+009F, U+FFFE and U+FFFF
/// raw, and folds U+0085 (NEL) into a space, so those are written as `\uXXXX`. Outside strings the text is ASCII, so
/// the pass touches nothing else.
fn document(value: &impl Serialize) -> Result<String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| Error::new(ErrorKind::Internal, format!("cannot write the cloud-init seed: {e}")))?;
    let mut out = String::with_capacity(text.len() + 1);
    for c in text.chars() {
        if matches!(c, '\u{7f}'..='\u{9f}' | '\u{fffe}' | '\u{ffff}') {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out.push('\n');
    Ok(out)
}
