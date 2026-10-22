//! The in-memory recipe: what a `*.devvm.yaml` file means.

use std::path::PathBuf;

use crate::{Arch, RunAs, Sha256};

/// A value that may differ per architecture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PerArch<T> {
    pub amd64: Option<T>,
    pub arm64: Option<T>,
}

impl<T> PerArch<T> {
    /// The same value for both architectures.
    pub fn any(value: T) -> Self
    where
        T: Clone,
    {
        PerArch {
            amd64: Some(value.clone()),
            arm64: Some(value),
        }
    }

    pub fn get(&self, arch: Arch) -> Option<&T> {
        match arch {
            Arch::Amd64 => self.amd64.as_ref(),
            Arch::Arm64 => self.arm64.as_ref(),
        }
    }
}

/// A download pinned by content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pin {
    pub url: String,
    pub sha256: Sha256,
}

/// What a recipe starts from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// Another recipe file; the path is already resolved against the declaring file's directory.
    Recipe(PathBuf),
    /// A base recipe embedded in devvm.
    Embedded(String),
    DiskImage(PerArch<Pin>),
    Oci {
        reference: String,
        digest: String,
    },
    /// A Windows installer ISO; the path is already resolved against the declaring file's directory.
    Iso {
        path: PathBuf,
    },
}

/// The shell a `run` step's command is written for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Sh,
    Bash,
    Pwsh,
    Cmd,
}

/// Where a `copy` step takes its files from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFrom {
    /// The files git tracks that exist on disk.
    GitTracked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UacMode {
    AutoApprove,
}

/// A known OS toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Setting {
    Uac(UacMode),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Run {
        command: String,
        shell: Option<Shell>,
        run_as: RunAs,
        cwd: Option<String>,
    },
    Copy {
        from: CopyFrom,
        to: String,
        read_only: bool,
    },
    Set(Setting),
}

/// Guest resources; unset fields take the defaults. Sizes are bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Resources {
    pub cpus: Option<u32>,
    pub memory: Option<u64>,
    pub disk: Option<u64>,
}

/// One parsed recipe file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recipe {
    pub name: String,
    pub source: Source,
    pub resources: Resources,
    pub steps: Vec<Step>,
    /// The file this was parsed from (a synthetic name for an embedded recipe).
    pub path: PathBuf,
    /// The directory of the file; relative paths in it were resolved against this.
    pub dir: PathBuf,
}
