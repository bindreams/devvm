//! User configuration: `<config>/config.yaml`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Dirs, Error, ErrorKind, Result};

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;

/// User settings. Every field is optional; a missing file is the default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Use this QEMU instead of devvm's own build.
    pub qemu_path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    qemu: Option<RawQemu>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQemu {
    path: Option<PathBuf>,
}

impl Config {
    /// Reads `<config>/config.yaml`.
    pub fn load(dirs: &Dirs) -> Result<Config> {
        let path = dirs.config.join("config.yaml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text, &path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(Error::io(ErrorKind::Io, format!("cannot read {}", path.display()), e)),
        }
    }

    /// Parses config text; `origin` names the file in errors. Unknown keys are errors.
    pub fn parse(text: &str, origin: &Path) -> Result<Config> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let raw: Option<Raw> = yaml_serde::from_str(text).map_err(|e| {
            Error::new(ErrorKind::Config, format!("invalid config {}: {e}", origin.display())).with_source(e)
        })?;
        Ok(Config {
            qemu_path: raw.and_then(|raw| raw.qemu).and_then(|qemu| qemu.path),
        })
    }
}
