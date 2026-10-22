//! Base recipes compiled into devvm, each pinning a URL and hash; versioned with devvm.

use std::path::Path;

use super::Recipe;

#[cfg(test)]
#[path = "embedded_tests.rs"]
mod embedded_tests;

const RECIPES: &[(&str, &str)] = &[("ubuntu-24.04", include_str!("embedded/ubuntu-24.04.devvm.yaml"))];

/// The names of the embedded recipes.
pub fn names() -> &'static [&'static str] {
    const NAMES: [&str; RECIPES.len()] = {
        let mut names = [""; RECIPES.len()];
        let mut i = 0;
        while i < RECIPES.len() {
            names[i] = RECIPES[i].0;
            i += 1;
        }
        names
    };
    &NAMES
}

/// The embedded recipe called `name`.
///
/// # Panics
/// If an embedded recipe does not parse; a test guards that.
pub fn get(name: &str) -> Option<Recipe> {
    let (_, text) = RECIPES.iter().find(|(known, _)| *known == name)?;
    let origin = format!("{name}.devvm.yaml");
    Some(Recipe::parse(text, Path::new(&origin)).expect("embedded recipes parse"))
}
