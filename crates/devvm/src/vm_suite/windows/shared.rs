//! What the lib's Windows suite and `devvm-cli`'s `vm_cli` do the same way when they build a Windows base. `vm_cli`
//! includes this file by `#[path]`, so it needs nothing from the lib.

use std::path::Path;
use std::sync::OnceLock;

/// Runs `make` unless an earlier call to it, with the same `slot`, failed: skuld runs a fixture's setup again after it
/// fails, and each attempt is a full Windows install or boot, so the first failure stands for every later test.
pub fn first_try<T>(slot: &OnceLock<String>, make: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    if let Some(error) = slot.get() {
        return Err(format!("the first attempt of this process failed: {error}"));
    }
    make().inspect_err(|error| {
        let _ = slot.set(error.clone());
    })
}

/// The text of a recipe that installs Windows from `iso` and runs `steps`, each one step's YAML.
pub fn recipe_text(iso: &Path, steps: &[&str]) -> Result<String, String> {
    let iso = iso.to_str().ok_or("a UTF-8 ISO path")?;
    let iso = serde_json::to_string(iso).map_err(|e| e.to_string())?;
    let steps = if steps.is_empty() {
        "steps: []\n".to_owned()
    } else {
        format!(
            "steps:\n{}",
            steps.iter().map(|step| format!("  - {step}\n")).collect::<String>()
        )
    };
    Ok(format!("from: {{ iso: {iso} }}\n{steps}"))
}
