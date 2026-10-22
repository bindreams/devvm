//! Recipes: the model, the YAML format, chains, embedded bases and layer keys.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::{Error, ErrorKind, GuestKind, Result};

pub mod embedded;
pub mod file;
pub mod hash;
pub mod model;

pub use model::{CopyFrom, PerArch, Pin, Recipe, Resources, Setting, Shell, Source, Step, UacMode};

#[cfg(test)]
#[path = "recipe_tests.rs"]
mod recipe_tests;

/// One step of a flattened chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlatStep {
    pub step: Step,
    /// The directory of the recipe that declared the step.
    pub origin_dir: PathBuf,
}

/// A recipe with its `from:` chain resolved down to a root source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flattened {
    pub name: String,
    pub guest: GuestKind,
    /// A disk image, OCI image or ISO; never `Recipe` or `Embedded`.
    pub root: Source,
    pub resources: Resources,
    /// Ancestors' steps first, the root-most recipe's first.
    pub steps: Vec<FlatStep>,
}

/// The identity of a recipe file in a `from:` chain: its canonical path.
fn chain_key(path: &Path) -> Result<String> {
    std::fs::canonicalize(path)
        .map(|canonical| canonical.display().to_string())
        .map_err(|e| Error::io(ErrorKind::Io, format!("cannot resolve {}", path.display()), e))
}

impl Recipe {
    /// The embedded base recipe called `name`; `NotFound` lists the known names.
    pub fn embedded(name: &str) -> Result<Recipe> {
        embedded::get(name).ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!(
                    "no embedded recipe named `{name}` (known: {}); a recipe file reference ends in `.devvm.yaml`",
                    embedded::names().join(", ")
                ),
            )
        })
    }

    /// What a user typed: a path to a `*.devvm.yaml` file (relative to `cwd`), else an embedded recipe name.
    pub fn from_ref(reference: &str, cwd: &Path) -> Result<Recipe> {
        if reference.ends_with(".devvm.yaml") {
            Recipe::from_file(&cwd.join(reference))
        } else {
            Recipe::embedded(reference)
        }
    }

    /// Resolves the `from:` chain. A cycle is an error naming the loop.
    pub fn flatten(&self) -> Result<Flattened> {
        let mut chain: Vec<Recipe> = vec![self.clone()];
        let mut visited: HashSet<String> = HashSet::new();
        let mut trail: Vec<String> = Vec::new();
        let root = loop {
            let current = chain.last().expect("the chain is never empty");
            let (next, key, label) = match &current.source {
                Source::Recipe(path) => {
                    let parent = Recipe::from_file(path).map_err(|e| {
                        let message = format!(
                            "{} starts from {}: {}",
                            current.path.display(),
                            path.display(),
                            e.message()
                        );
                        Error::new(e.kind(), message)
                    })?;
                    let key = chain_key(path)?;
                    (parent, key, path.display().to_string())
                }
                Source::Embedded(name) => (Recipe::embedded(name)?, format!("embedded:{name}"), name.clone()),
                other => break other.clone(),
            };
            trail.push(label);
            if !visited.insert(key) {
                return Err(Error::new(
                    ErrorKind::Recipe,
                    format!("recipe cycle: {}", trail.join(" -> ")),
                ));
            }
            chain.push(next);
        };

        let guest = match &root {
            Source::Iso { .. } => GuestKind::Windows,
            Source::Oci { .. } => GuestKind::Macos,
            _ => GuestKind::Linux,
        };
        let pick = |field: fn(&Resources) -> Option<u64>| chain.iter().find_map(|recipe| field(&recipe.resources));
        let resources = Resources {
            cpus: chain.iter().find_map(|recipe| recipe.resources.cpus),
            memory: pick(|r| r.memory),
            disk: pick(|r| r.disk),
        };
        let steps = chain
            .iter()
            .rev()
            .flat_map(|recipe| {
                recipe.steps.iter().map(|step| FlatStep {
                    step: step.clone(),
                    origin_dir: recipe.dir.clone(),
                })
            })
            .collect();
        Ok(Flattened {
            name: self.name.clone(),
            guest,
            root,
            resources,
            steps,
        })
    }
}
