//! The files of the base build's answer disk: the answer file, WinPE's script, the first-logon script, the boot task and
//! the elevated helper's runner, and the configs the first-logon script installs.
//!
//! They are text in `templates/`, with `@@NAME@@` where devvm fills a value in. A value is quoted for the kind of file
//! it lands in (XML, a PowerShell or a YAML string), so that none of them can end its own string; `@@!NAME@@` is
//! verbatim, for a value that is a statement. The paths and names the host's code also uses (`elevate.rs`, `wrap.rs`)
//! are filled in from those constants, never copied. Every pin is a value too, so the definition hash sees it.

use sha2::{Digest, Sha256};

use super::elevate::{CALLER, CLAIM, DONE, EXIT, READ, ROOT, SCRIPT, STDERR, STDOUT, TASK};
use super::pins::{ALL, OPENSSH, POWERSHELL, Pin, SSH_BROKER};
use super::wrap::{BROKER_EXE, PWSH_EXE, literal};
use super::{BOOT_ID_FILE, UTF8_OUTPUT, WindowsGuest};
use crate::guest::GuestOs;
use crate::media::SeedFile;
use crate::{Error, ErrorKind, Result};

#[cfg(test)]
#[path = "templates_tests.rs"]
mod templates_tests;

/// The guest's data directory: shared by SYSTEM and `user`, so its ACL is explicit (the first-logon script sets it).
pub(super) const DEVVM_DIR: &str = r"C:\ProgramData\devvm";
/// The time zone of every base: the guest's clock is the host's UTC clock.
pub(super) const TIME_ZONE: &str = "UTC";
/// The computer name of every base; an instance keeps its base's.
pub(super) const COMPUTER_NAME: &str = "DEVVM";

/// What the answer disk's `logs/` folder holds when Setup runs, named: `winpe.cmd` copies these and nothing else, and a
/// path the guest did not write is absent from what the host reads.
pub(super) const LOG_FILES: &[&str] = &[
    "logs/winpe.log",
    "logs/setupact.log",
    "logs/setuperr.log",
    "logs/setupapi.dev.log",
];

/// The C# source of the guest's native helper (the runner's job and caller handling, a directory flush) and the assembly
/// the bootstrap compiles it to, in the scripts directory. It is compiled once, at the base build.
pub(super) const NATIVE_SOURCE: &str = "Devvm.cs";
pub(super) const NATIVE_DLL: &str = "Devvm.dll";

/// The answer disk's files, by the name they have there.
pub(super) const TEMPLATES: &[(&str, &str)] = &[
    ("Autounattend.xml", include_str!("templates/Autounattend.xml")),
    (NATIVE_SOURCE, include_str!("templates/Devvm.cs")),
    ("winpe.cmd", include_str!("templates/winpe.cmd")),
    ("lib.ps1", include_str!("templates/lib.ps1")),
    ("bootstrap.ps1", include_str!("templates/bootstrap.ps1")),
    ("boot-task.ps1", include_str!("templates/boot-task.ps1")),
    ("eb-runner.ps1", include_str!("templates/eb-runner.ps1")),
    ("sshd_config", include_str!("templates/sshd_config")),
    ("broker-config.yaml", include_str!("templates/broker-config.yaml")),
];

/// What differs between two builds' answer files.
pub(super) struct AnswerInputs<'a> {
    /// The account's password and the autologon's: random per build, never stored by devvm.
    pub password: &'a str,
    /// The index of the image to install, in the ISO's image list.
    pub image_index: u32,
    /// The image's default language, as a locale name (`en-US`): the installed system's, not English.
    pub language: &'a str,
    pub time_zone: &'a str,
    pub computer_name: &'a str,
}

/// The answer file for `inputs`.
pub(super) fn answer_file(inputs: &AnswerInputs) -> Result<String> {
    let (name, text) = TEMPLATES[0];
    debug_assert_eq!(name, "Autounattend.xml");
    render(name, text, &values(inputs))
}

/// Every file of the answer disk for `inputs`, ready to write to it.
pub(super) fn answer_disk_files(inputs: &AnswerInputs) -> Result<Vec<SeedFile>> {
    disk_files(TEMPLATES, &values(inputs))
}

/// The files of `templates` with `values` filled in, as they are written to the answer disk.
pub(super) fn disk_files(templates: &[(&str, &str)], values: &[(&str, String)]) -> Result<Vec<SeedFile>> {
    templates
        .iter()
        .map(|(name, text)| {
            let mut text = render(name, text, values)?;
            if name.ends_with(".cmd") {
                // cmd.exe finds its labels reliably only in a file with CRLF line ends.
                text = text.replace("\r\n", "\n").replace('\n', "\r\n");
            }
            Ok(SeedFile {
                name: (*name).to_owned(),
                bytes: text.into_bytes(),
            })
        })
        .collect()
}

/// What the definition hash renders the templates with. The password is a fixed stand-in (the build's is random and
/// never stored); the rest is what a build of any ISO shares: the image and its language come from the ISO, which is
/// part of the base's key already.
pub(super) fn probe_inputs() -> AnswerInputs<'static> {
    AnswerInputs {
        password: "definition-hash-probe",
        image_index: 1,
        language: "en-US",
        time_zone: TIME_ZONE,
        computer_name: COMPUTER_NAME,
    }
}

/// Hash of everything that decides what the base is: every file of the answer disk as a build renders it (so every
/// constant a template substitutes is in it), every pin and the guest's semantics version.
pub(super) fn definition_hash() -> [u8; 32] {
    let files = answer_disk_files(&probe_inputs()).expect("the Windows templates render (checked by a test)");
    definition_hash_of(&files, ALL, WindowsGuest.semantics_version())
}

/// [`definition_hash`] of the given parts. Fields are length-prefixed and the lists counted, so no two definitions share
/// a byte stream.
pub(super) fn definition_hash_of(files: &[SeedFile], pins: &[&Pin], semantics_version: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    let mut field = |bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(b"devvm windows base definition 2");
    field(&semantics_version.to_le_bytes());
    field(&(files.len() as u64).to_le_bytes());
    for file in files {
        field(file.name.as_bytes());
        field(&file.bytes);
    }
    field(&(pins.len() as u64).to_le_bytes());
    for pin in pins {
        field(pin.what.as_bytes());
        field(pin.url.as_bytes());
        field(pin.sha256.as_bytes());
    }
    hasher.finalize().into()
}

// Rendering ===========================================================================================================

/// How a value is quoted for the file it goes into.
#[derive(Clone, Copy)]
enum Quoting {
    Xml,
    PowerShell,
    Yaml,
    Verbatim,
}

impl Quoting {
    fn of(file: &str) -> Quoting {
        match file.rsplit('.').next() {
            Some("xml") => Quoting::Xml,
            Some("ps1") => Quoting::PowerShell,
            Some("yaml") => Quoting::Yaml,
            _ => Quoting::Verbatim,
        }
    }

    fn apply(self, value: &str) -> String {
        match self {
            Quoting::Xml => quick_xml::escape::escape(value).into_owned(),
            // A whole string literal, quotes included.
            Quoting::PowerShell => literal(value),
            Quoting::Yaml => format!("'{}'", value.replace('\'', "''")),
            Quoting::Verbatim => value.to_owned(),
        }
    }
}

/// The virtio-win CD's driver folders on every letter the CD can have, as the answer file's `PathAndCredentials`
/// entries; Setup skips a path that is not there. `winpe.cmd` finds the CD by file over the same letters.
fn driver_paths(key_prefix: &str) -> String {
    let mut entries = Vec::new();
    for letter in 'C'..='Z' {
        for driver in ["viostor", "NetKVM"] {
            let key = format!("{key_prefix}{}", entries.len() + 1);
            entries.push(format!(
                r#"<PathAndCredentials wcm:action="add" wcm:keyValue="{key}"><Path>{letter}:\{driver}\w11\amd64</Path></PathAndCredentials>"#
            ));
        }
    }
    entries.join("\n        ")
}

/// Every value a template can name.
pub(super) fn values(inputs: &AnswerInputs) -> Vec<(&'static str, String)> {
    let values: Vec<(&'static str, String)> = vec![
        ("PASSWORD", inputs.password.to_owned()),
        ("IMAGE_INDEX", inputs.image_index.to_string()),
        ("LANGUAGE", inputs.language.to_owned()),
        ("TIME_ZONE", inputs.time_zone.to_owned()),
        ("COMPUTER_NAME", inputs.computer_name.to_owned()),
        ("DRIVER_PATHS_OFFLINE", driver_paths("off")),
        ("DRIVER_PATHS_SPECIALIZE", driver_paths("os")),
        ("DEVVM_DIR", DEVVM_DIR.to_owned()),
        ("NATIVE_SOURCE", NATIVE_SOURCE.to_owned()),
        ("NATIVE_DLL", NATIVE_DLL.to_owned()),
        ("PWSH_EXE", PWSH_EXE.to_owned()),
        ("BROKER_EXE", BROKER_EXE.to_owned()),
        ("BOOT_ID_FILE", BOOT_ID_FILE.to_owned()),
        ("UTF8_OUTPUT", UTF8_OUTPUT.to_owned()),
        ("EB_ROOT", ROOT.to_owned()),
        ("EB_TASK", TASK.to_owned()),
        ("EB_SCRIPT", SCRIPT.to_owned()),
        ("EB_CALLER", CALLER.to_owned()),
        ("EB_CLAIM", CLAIM.to_owned()),
        ("EB_STDOUT", STDOUT.to_owned()),
        ("EB_STDERR", STDERR.to_owned()),
        ("EB_EXIT", EXIT.to_owned()),
        ("EB_DONE", DONE.to_owned()),
        ("EB_READ", READ.to_owned()),
        ("OPENSSH_URL", OPENSSH.url.to_owned()),
        ("OPENSSH_SHA256", OPENSSH.sha256.to_owned()),
        ("POWERSHELL_URL", POWERSHELL.url.to_owned()),
        ("POWERSHELL_SHA256", POWERSHELL.sha256.to_owned()),
        ("SSH_BROKER_URL", SSH_BROKER.url.to_owned()),
        ("SSH_BROKER_SHA256", SSH_BROKER.sha256.to_owned()),
    ];
    values
}

/// `text` with every `@@NAME@@` replaced, in one pass: a value is never scanned for placeholders. A name no value has,
/// or an `@@` that is not closed, is an `internal` error naming the template: a template and its values are one change.
fn render(file: &str, text: &str, values: &[(&str, String)]) -> Result<String> {
    let quoting = Quoting::of(file);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("@@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("@@") else {
            return Err(unfilled(file, "an `@@` that is never closed"));
        };
        let (verbatim, name) = match after[..end].strip_prefix('!') {
            Some(name) => (true, name),
            None => (false, &after[..end]),
        };
        let Some((_, value)) = values.iter().find(|(known, _)| *known == name) else {
            return Err(unfilled(
                file,
                &format!("the placeholder `@@{name}@@` that has no value"),
            ));
        };
        out.push_str(&if verbatim { value.clone() } else { quoting.apply(value) });
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

fn unfilled(file: &str, what: &str) -> Error {
    Error::new(ErrorKind::Internal, format!("the Windows template {file} has {what}"))
}
