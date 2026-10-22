//! Recipes: the model, the YAML format, chains, embedded bases and layer keys.

pub mod file;
pub mod model;

pub use model::{CopyFrom, PerArch, Pin, Recipe, Resources, Setting, Shell, Source, Step, UacMode};
