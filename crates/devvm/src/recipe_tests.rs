use std::path::{Path, PathBuf};

use crate::recipe::embedded;
use crate::recipe::{Recipe, Source, Step};
use crate::test_support::fixtures::unicode_home;
use crate::{ErrorKind, GuestKind};

fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn run(command: &str) -> Step {
    Step::Run {
        command: command.into(),
        shell: None,
        run_as: crate::RunAs::User,
        cwd: None,
    }
}

fn commands(flat: &crate::recipe::Flattened) -> Vec<String> {
    flat.steps
        .iter()
        .map(|flat| match &flat.step {
            Step::Run { command, .. } => command.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[skuld::test]
fn child_inherits_parent_steps_in_order(#[fixture(unicode_home)] home: &Path) {
    write(
        home,
        "a b/base.devvm.yaml",
        "from: ubuntu-24.04\nsteps:\n  - run: one\n  - run: two\n",
    );
    let child = write(
        home,
        "kids/child.devvm.yaml",
        "from: ../a b/base.devvm.yaml\nsteps:\n  - run: three\n",
    );
    let flat = Recipe::from_file(&child).unwrap().flatten().unwrap();
    assert_eq!(commands(&flat), ["one", "two", "three"]);
    assert_eq!(flat.name, "child");
    assert_eq!(flat.steps[0].step, run("one"));
    // The directory keeps the `..` it was reached by; both name the same place.
    let same = |a: &Path, b: &Path| std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap();
    assert!(same(&flat.steps[0].origin_dir, &home.join("a b")));
    assert!(same(&flat.steps[2].origin_dir, &home.join("kids")));
    assert!(matches!(flat.root, Source::DiskImage(_)));
}

#[skuld::test]
fn leaf_resources_override_per_field(#[fixture(unicode_home)] home: &Path) {
    write(
        home,
        "base.devvm.yaml",
        "from: ubuntu-24.04\nresources: { cpus: 2, memory: 4GiB, disk: 20GiB }\n",
    );
    let child = write(
        home,
        "child.devvm.yaml",
        "from: base.devvm.yaml\nresources: { cpus: 8 }\n",
    );
    let flat = Recipe::from_file(&child).unwrap().flatten().unwrap();
    assert_eq!(flat.resources.cpus, Some(8));
    assert_eq!(flat.resources.memory, Some(4 << 30));
    assert_eq!(flat.resources.disk, Some(20 << 30));
}

#[skuld::test]
fn disk_override_comes_from_the_leaf_for_the_whole_chain(#[fixture(unicode_home)] home: &Path) {
    let base = write(
        home,
        "base.devvm.yaml",
        "from: ubuntu-24.04\nresources: { disk: 20GiB }\nsteps:\n  - run: p\n",
    );
    let child = write(
        home,
        "child.devvm.yaml",
        "from: base.devvm.yaml\nresources: { disk: 40GiB }\nsteps:\n  - run: c\n",
    );
    assert_eq!(
        Recipe::from_file(&base).unwrap().flatten().unwrap().resources.disk,
        Some(20 << 30)
    );
    let flat = Recipe::from_file(&child).unwrap().flatten().unwrap();
    assert_eq!(flat.resources.disk, Some(40 << 30));
    assert_eq!(commands(&flat), ["p", "c"]);
}

#[skuld::test]
fn cycle_is_an_error_naming_the_loop(#[fixture(unicode_home)] home: &Path) {
    let a = write(home, "a.devvm.yaml", "from: b.devvm.yaml\n");
    write(home, "b.devvm.yaml", "from: a.devvm.yaml\n");
    let error = Recipe::from_file(&a).unwrap().flatten().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("cycle"), "{error}");
    assert!(
        error.message().contains("a.devvm.yaml") && error.message().contains("b.devvm.yaml"),
        "{error}"
    );

    let me = write(home, "me.devvm.yaml", "from: me.devvm.yaml\n");
    let error = Recipe::from_file(&me).unwrap().flatten().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(
        error.message().contains("cycle") && error.message().contains("me.devvm.yaml"),
        "{error}"
    );
}

#[skuld::test]
fn from_ref_distinguishes_path_from_embedded_name(#[fixture(unicode_home)] home: &Path) {
    // A `.devvm.yaml` reference is a path relative to cwd, never an embedded name.
    let error = Recipe::from_ref("ubuntu-24.04.devvm.yaml", home).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.message().contains("ubuntu-24.04.devvm.yaml"), "{error}");
    write(home, "x y/mine.devvm.yaml", "from: ubuntu-24.04\nname: mine\n");
    assert_eq!(Recipe::from_ref("x y/mine.devvm.yaml", home).unwrap().name, "mine");

    // Anything else is an embedded name, even when a file of that name exists in cwd.
    write(home, "ubuntu-24.04", "not a recipe");
    let recipe = Recipe::from_ref("ubuntu-24.04", home).unwrap();
    assert_eq!(recipe.name, "ubuntu-24.04");
}

#[skuld::test]
fn unknown_embedded_name_lists_known_names() {
    let error = Recipe::embedded("ubuntu-99").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.message().contains("ubuntu-99"), "{error}");
    for name in embedded::names() {
        assert!(error.message().contains(name), "{error}");
    }
}

#[skuld::test]
fn missing_parent_file_names_both_files(#[fixture(unicode_home)] home: &Path) {
    let child = write(home, "child.devvm.yaml", "from: gone.devvm.yaml\n");
    let error = Recipe::from_file(&child).unwrap().flatten().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.message().contains("child.devvm.yaml"), "{error}");
    assert!(error.message().contains("gone.devvm.yaml"), "{error}");
}

#[skuld::test]
fn guest_kind_follows_the_root(#[fixture(unicode_home)] home: &Path) {
    let iso = write(
        home,
        "w.devvm.yaml",
        "from: { iso: win.iso }\nsteps:\n  - run: x\n    shell: pwsh\n",
    );
    let flat = Recipe::from_file(&iso).unwrap().flatten().unwrap();
    assert_eq!(flat.guest, GuestKind::Windows);
    assert_eq!(
        flat.root,
        Source::Iso {
            path: home.join("win.iso")
        }
    );

    let sha = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let oci = write(
        home,
        "m.devvm.yaml",
        &format!("from: {{ oci: r/m, digest: 'sha256:{sha}' }}\n"),
    );
    assert_eq!(
        Recipe::from_file(&oci).unwrap().flatten().unwrap().guest,
        GuestKind::Macos
    );

    let flat = Recipe::embedded("ubuntu-24.04").unwrap().flatten().unwrap();
    assert_eq!(flat.guest, GuestKind::Linux);
    assert!(flat.steps.is_empty());
}

#[skuld::test]
fn every_embedded_recipe_parses_and_flattens() {
    for name in embedded::names() {
        let recipe = embedded::get(name).unwrap_or_else(|| panic!("{name} is listed but missing"));
        assert_eq!(recipe.name, *name);
        recipe.flatten().unwrap();
    }
    assert!(embedded::get("nope").is_none());
}

#[skuld::test]
fn flattened_and_recipe_are_send_and_sync() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<Recipe>();
    assert_bounds::<crate::recipe::Flattened>();
}

#[skuld::test]
fn a_recipe_path_that_cannot_be_canonicalized_is_an_error_not_a_fallback(#[fixture(unicode_home)] home: &Path) {
    // Falling back to the spelled path would let two spellings of one file defeat cycle detection.
    let error = crate::recipe::chain_key(&home.join("gone.devvm.yaml")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("gone.devvm.yaml"), "{error}");
}
