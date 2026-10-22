//! Parsing and validating `*.devvm.yaml` text.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::Error as _;

use super::model::{CopyFrom, PerArch, Pin, Recipe, Resources, Setting, Shell, Source, Step, UacMode};
use crate::{Error, ErrorKind, Result, RunAs, Sha256};

#[cfg(test)]
#[path = "file_tests.rs"]
mod file_tests;

const SUFFIX: &str = ".devvm.yaml";

// Raw serde shapes ====================================================================================================

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipe {
    name: Option<String>,
    from: RawFrom,
    #[serde(default)]
    resources: RawResources,
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResources {
    cpus: Option<u32>,
    memory: Option<Size>,
    disk: Option<Size>,
}

/// A byte count written as `8GiB` or as a plain integer.
struct Size(u64);

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Size, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Text {
            Bytes(u64),
            Text(String),
        }
        match Text::deserialize(deserializer)? {
            Text::Bytes(bytes) => Ok(Size(bytes)),
            Text::Text(text) => text
                .parse::<bytesize::ByteSize>()
                .map(|size| Size(size.as_u64()))
                .map_err(|e| D::Error::custom(format!("invalid size `{text}`: {e}"))),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPin {
    url: String,
    sha256: Sha256,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPerArch {
    amd64: Option<RawPin>,
    arm64: Option<RawPin>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOci {
    oci: String,
    digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIso {
    iso: PathBuf,
}

/// `from:`: a string, or one of four mappings told apart by their keys.
enum RawFrom {
    Name(String),
    Iso(RawIso),
    Oci(RawOci),
    Pin(RawPin),
    PerArch(RawPerArch),
}

impl<'de> Deserialize<'de> for RawFrom {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<RawFrom, D::Error> {
        let value = yaml_serde::Value::deserialize(deserializer)?;
        let wrong = |e: yaml_serde::Error| D::Error::custom(format!("invalid `from`: {e}"));
        match &value {
            yaml_serde::Value::String(name) => Ok(RawFrom::Name(name.clone())),
            yaml_serde::Value::Mapping(map) => {
                let has = |key: &str| map.contains_key(key);
                if has("iso") {
                    yaml_serde::from_value(value).map(RawFrom::Iso).map_err(wrong)
                } else if has("oci") || has("digest") {
                    yaml_serde::from_value(value).map(RawFrom::Oci).map_err(wrong)
                } else if has("url") || has("sha256") {
                    yaml_serde::from_value(value).map(RawFrom::Pin).map_err(wrong)
                } else if has("amd64") || has("arm64") {
                    yaml_serde::from_value(value).map(RawFrom::PerArch).map_err(wrong)
                } else {
                    Err(D::Error::custom(
                        "invalid `from`: expected one of `iso`, `oci` with `digest`, `url` with `sha256`, or `amd64` / `arm64`",
                    ))
                }
            }
            _ => Err(D::Error::custom(
                "invalid `from`: expected a recipe path, an embedded name, or a mapping",
            )),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    run: Option<String>,
    shell: Option<RawShell>,
    elevated: Option<bool>,
    cwd: Option<String>,
    copy: Option<RawCopy>,
    set: Option<RawSet>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawShell {
    Sh,
    Bash,
    Pwsh,
    Cmd,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCopy {
    from: String,
    to: String,
    #[serde(rename = "read-only")]
    read_only: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSet {
    uac: Option<String>,
}

// Validation ==========================================================================================================

fn invalid(origin: &Path, detail: impl std::fmt::Display) -> Error {
    Error::new(
        ErrorKind::Recipe,
        format!("invalid recipe {}: {detail}", origin.display()),
    )
}

fn pin(raw: RawPin) -> Pin {
    Pin {
        url: raw.url,
        sha256: raw.sha256,
    }
}

fn source(raw: RawFrom, dir: &Path, origin: &Path) -> Result<Source> {
    Ok(match raw {
        RawFrom::Name(name) if name.ends_with(SUFFIX) => Source::Recipe(dir.join(name)),
        RawFrom::Name(name) => Source::Embedded(name),
        RawFrom::Iso(iso) => Source::Iso {
            path: dir.join(iso.iso),
        },
        RawFrom::Oci(oci) => {
            let valid = oci
                .digest
                .strip_prefix("sha256:")
                .is_some_and(|hex| hex.parse::<Sha256>().is_ok());
            if !valid {
                return Err(invalid(
                    origin,
                    format!("`digest` must be `sha256:` and 64 hex digits, got `{}`", oci.digest),
                ));
            }
            Source::Oci {
                reference: oci.oci,
                digest: oci.digest,
            }
        }
        RawFrom::Pin(raw) => Source::DiskImage(PerArch::any(pin(raw))),
        RawFrom::PerArch(raw) => Source::DiskImage(PerArch {
            amd64: raw.amd64.map(pin),
            arm64: raw.arm64.map(pin),
        }),
    })
}

fn step(raw: RawStep, origin: &Path) -> Result<Step> {
    let actions = [raw.run.is_some(), raw.copy.is_some(), raw.set.is_some()];
    if actions.iter().filter(|present| **present).count() != 1 {
        return Err(invalid(origin, "a step needs exactly one of `run`, `copy` or `set`"));
    }
    let modifiers = [
        ("shell", raw.shell.is_some()),
        ("elevated", raw.elevated.is_some()),
        ("cwd", raw.cwd.is_some()),
    ];
    if let Some(command) = raw.run {
        let shell = raw.shell.map(|shell| match shell {
            RawShell::Sh => Shell::Sh,
            RawShell::Bash => Shell::Bash,
            RawShell::Pwsh => Shell::Pwsh,
            RawShell::Cmd => Shell::Cmd,
        });
        let run_as = if raw.elevated.unwrap_or(false) {
            RunAs::Elevated
        } else {
            RunAs::User
        };
        return Ok(Step::Run {
            command,
            shell,
            run_as,
            cwd: raw.cwd,
        });
    }
    if let Some((name, _)) = modifiers.iter().find(|(_, present)| *present) {
        return Err(invalid(origin, format!("`{name}` is only valid on `run` steps")));
    }
    if let Some(copy) = raw.copy {
        if copy.from != "git-tracked" {
            return Err(invalid(
                origin,
                format!("`copy.from` must be `git-tracked`, got `{}`", copy.from),
            ));
        }
        return Ok(Step::Copy {
            from: CopyFrom::GitTracked,
            to: copy.to,
            read_only: copy.read_only.unwrap_or(false),
        });
    }
    let set = raw.set.expect("exactly one action is present");
    match set.uac.as_deref() {
        Some("auto-approve") => Ok(Step::Set(Setting::Uac(UacMode::AutoApprove))),
        Some(other) => Err(invalid(
            origin,
            format!("`set.uac` must be `auto-approve`, got `{other}`"),
        )),
        None => Err(invalid(origin, "`set` needs `uac`")),
    }
}

// Entry points ========================================================================================================

impl Recipe {
    /// Parses recipe text. `origin` is the file's path: it names the file in errors, sets the directory relative paths
    /// resolve against, and gives the default name.
    pub fn parse(text: &str, origin: &Path) -> Result<Recipe> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let raw: RawRecipe = yaml_serde::from_str(text).map_err(|e| invalid(origin, &e).with_source(e))?;
        let dir = origin.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = raw.name.unwrap_or_else(|| default_name(origin));
        let steps = raw
            .steps
            .into_iter()
            .map(|raw| step(raw, origin))
            .collect::<Result<Vec<_>>>()?;
        Ok(Recipe {
            name,
            source: source(raw.from, &dir, origin)?,
            resources: Resources {
                cpus: raw.resources.cpus,
                memory: raw.resources.memory.map(|size| size.0),
                disk: raw.resources.disk.map(|size| size.0),
            },
            steps,
            dir,
        })
    }

    /// Reads and parses a recipe file.
    pub fn from_file(path: &Path) -> Result<Recipe> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!("recipe file {} does not exist", path.display()),
                ));
            }
            Err(e) => return Err(Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e)),
        };
        Recipe::parse(&text, path)
    }
}

fn default_name(origin: &Path) -> String {
    let file = origin.file_name().map(|name| name.to_string_lossy().into_owned());
    match file {
        Some(file) => file.strip_suffix(SUFFIX).map(str::to_owned).unwrap_or_else(|| {
            origin
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or(file)
        }),
        None => "recipe".to_owned(),
    }
}
