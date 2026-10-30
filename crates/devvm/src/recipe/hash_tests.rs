use std::collections::HashSet;
use std::path::Path;

use crate::recipe::hash::{
    CopyManifest, EntryContent, KeyContext, LayerKey, ManifestEntry, RootContent, STEP_SEMANTICS_VERSION, chain_keys,
    root_key, root_key_with, step_key, step_key_with,
};
use crate::recipe::{CopyFrom, Recipe, Setting, Shell, Step, UacMode};
use crate::test_support::fixtures::unicode_home;
use crate::{Arch, ErrorKind, RunAs, Sha256};

fn sha(byte: u8) -> Sha256 {
    Sha256::from_bytes([byte; 32])
}

fn entry(path: &str, mode: u32, content: EntryContent) -> ManifestEntry {
    ManifestEntry {
        path: path.to_owned(),
        mode,
        content,
    }
}

fn file(path: &str, mode: u32, byte: u8) -> ManifestEntry {
    entry(path, mode, EntryContent::FileSha256(sha(byte)))
}

fn manifest(entries: Vec<ManifestEntry>) -> CopyManifest {
    CopyManifest::new(entries).unwrap()
}

fn run(command: &str, shell: Option<Shell>, run_as: RunAs, cwd: Option<&str>) -> Step {
    Step::Run {
        command: command.into(),
        shell,
        run_as,
        cwd: cwd.map(Into::into),
    }
}

fn copy(to: &str, read_only: bool) -> Step {
    Step::Copy {
        from: CopyFrom::GitTracked,
        to: to.into(),
        read_only,
    }
}

// Every input =========================================================================================================

/// A root, one run step and one copy step, with every input a parameter.
#[derive(Clone)]
struct Chain {
    arch: Arch,
    disk: Option<u64>,
    guest_semantics: u32,
    step_semantics: u32,
    content: RootContent,
    base_definition: [u8; 32],
    run: Step,
    copy: Step,
    manifest: CopyManifest,
}

impl Chain {
    fn base() -> Chain {
        Chain {
            arch: Arch::Amd64,
            disk: Some(40 << 30),
            guest_semantics: 1,
            step_semantics: STEP_SEMANTICS_VERSION,
            content: RootContent::Sha256(sha(1)),
            base_definition: [7; 32],
            run: run("make", Some(Shell::Bash), RunAs::User, Some("/src")),
            copy: copy("~/proj", false),
            manifest: manifest(vec![file("a.txt", 0o100644, 1), file("bin/run.sh", 0o100755, 2)]),
        }
    }

    fn ctx(&self) -> KeyContext {
        KeyContext {
            arch: self.arch,
            disk: self.disk,
            guest_semantics_version: self.guest_semantics,
        }
    }

    fn root(&self) -> LayerKey {
        root_key_with(&self.ctx(), &self.content, &self.base_definition, self.step_semantics)
    }

    fn run_key(&self) -> LayerKey {
        step_key_with(&self.ctx(), &self.root(), &self.run, None, self.step_semantics).unwrap()
    }

    fn last(&self) -> LayerKey {
        let manifest = matches!(self.copy, Step::Copy { .. }).then_some(&self.manifest);
        step_key_with(&self.ctx(), &self.run_key(), &self.copy, manifest, self.step_semantics).unwrap()
    }
}

#[skuld::test]
fn oci_digest_case_does_not_change_the_key() {
    let lower = format!("sha256:{}", sha(0xab));
    let mut chain = Chain::base();
    chain.content = RootContent::OciDigest(lower.clone());
    let expected = chain.root();
    chain.content = RootContent::OciDigest(lower.to_ascii_uppercase());
    assert_eq!(chain.root(), expected);
}

#[skuld::test]
fn every_key_input_changes_the_key() {
    let base = Chain::base();
    let mut variants: Vec<(&str, Chain)> = Vec::new();
    let mut vary = |name, change: &dyn Fn(&mut Chain)| {
        let mut chain = base.clone();
        change(&mut chain);
        variants.push((name, chain));
    };
    vary("source sha256", &|c| c.content = RootContent::Sha256(sha(2)));
    vary("oci digest vs equal sha256", &|c| {
        c.content = RootContent::OciDigest(sha(1).to_string());
    });
    vary("oci digest", &|c| {
        c.content = RootContent::OciDigest("sha256:other".into())
    });
    vary("base definition", &|c| c.base_definition = [8; 32]);
    vary("arch", &|c| c.arch = Arch::Arm64);
    vary("disk none", &|c| c.disk = None);
    vary("disk other", &|c| c.disk = Some(41 << 30));
    vary("step semantics", &|c| c.step_semantics = STEP_SEMANTICS_VERSION + 1);
    vary("guest semantics", &|c| c.guest_semantics = 2);
    vary("run command", &|c| {
        c.run = run("make all", Some(Shell::Bash), RunAs::User, Some("/src"))
    });
    vary("run shell none", &|c| {
        c.run = run("make", None, RunAs::User, Some("/src"))
    });
    vary("run shell other", &|c| {
        c.run = run("make", Some(Shell::Sh), RunAs::User, Some("/src"))
    });
    vary("run as", &|c| {
        c.run = run("make", Some(Shell::Bash), RunAs::Elevated, Some("/src"))
    });
    vary("run cwd none", &|c| {
        c.run = run("make", Some(Shell::Bash), RunAs::User, None)
    });
    vary("run cwd other", &|c| {
        c.run = run("make", Some(Shell::Bash), RunAs::User, Some("/tmp"))
    });
    vary("copy to", &|c| c.copy = copy("~/other", false));
    vary("copy read-only", &|c| c.copy = copy("~/proj", true));
    vary("copy replaced by set", &|c| {
        c.copy = Step::Set(Setting::Uac(UacMode::AutoApprove))
    });
    vary("manifest path", &|c| {
        c.manifest = manifest(vec![file("b.txt", 0o100644, 1), file("bin/run.sh", 0o100755, 2)])
    });
    vary("manifest mode", &|c| {
        c.manifest = manifest(vec![file("a.txt", 0o100755, 1), file("bin/run.sh", 0o100755, 2)]);
    });
    vary("manifest content", &|c| {
        c.manifest = manifest(vec![file("a.txt", 0o100644, 9), file("bin/run.sh", 0o100755, 2)]);
    });
    vary("manifest entry removed", &|c| {
        c.manifest = manifest(vec![file("a.txt", 0o100644, 1)])
    });
    vary("manifest symlink", &|c| {
        c.manifest = manifest(vec![
            file("a.txt", 0o100644, 1),
            entry("bin/run.sh", 0o120000, EntryContent::SymlinkTarget("../a.txt".into())),
        ]);
    });

    let mut seen = HashSet::from([base.last()]);
    for (name, chain) in &variants {
        assert!(seen.insert(chain.last()), "varying `{name}` did not give a new key");
    }
    assert_eq!(seen.len(), variants.len() + 1);
}

#[skuld::test]
fn root_and_step_keys_each_see_their_own_inputs() {
    let base = Chain::base();
    let root = base.root();
    for changed in [
        Chain {
            arch: Arch::Arm64,
            ..base.clone()
        },
        Chain {
            disk: None,
            ..base.clone()
        },
        Chain {
            guest_semantics: 9,
            ..base.clone()
        },
        Chain {
            step_semantics: 9,
            ..base.clone()
        },
        Chain {
            base_definition: [0; 32],
            ..base.clone()
        },
        Chain {
            content: RootContent::Sha256(sha(5)),
            ..base.clone()
        },
    ] {
        assert_ne!(changed.root(), root);
    }
    // The public entry points use the shipped constant.
    let ctx = base.ctx();
    assert_eq!(root_key(&ctx, &base.content, &base.base_definition), root);
    assert_eq!(step_key(&ctx, &root, &base.run, None).unwrap(), base.run_key());
    // A different parent gives a different step key.
    let other_parent = Chain {
        content: RootContent::Sha256(sha(5)),
        ..base.clone()
    }
    .root();
    assert_ne!(step_key(&ctx, &other_parent, &base.run, None).unwrap(), base.run_key());
}

#[skuld::test]
fn step_key_without_a_manifest_for_copy_is_internal_error() {
    let base = Chain::base();
    let error = step_key(&base.ctx(), &base.root(), &base.copy, None).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
}

#[skuld::test]
fn step_key_with_a_manifest_for_run_is_internal_error() {
    let base = Chain::base();
    let error = step_key(&base.ctx(), &base.root(), &base.run, Some(&base.manifest)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Internal);
    let set = Step::Set(Setting::Uac(UacMode::AutoApprove));
    assert_eq!(
        step_key(&base.ctx(), &base.root(), &set, Some(&base.manifest))
            .unwrap_err()
            .kind(),
        ErrorKind::Internal
    );
}

// Framing =============================================================================================================

#[skuld::test]
fn field_splits_are_unambiguous() {
    let base = Chain::base();
    let key_of =
        |step: Step, manifest: Option<&CopyManifest>| step_key(&base.ctx(), &base.root(), &step, manifest).unwrap();

    let a = key_of(run("ab", None, RunAs::User, Some("c")), None);
    let b = key_of(run("a", None, RunAs::User, Some("bc")), None);
    assert_ne!(a, b);
    assert_ne!(
        key_of(run("", None, RunAs::User, None), None),
        key_of(run("", None, RunAs::User, Some("")), None)
    );

    let m_c = manifest(vec![file("c", 0o100644, 1)]);
    let m_bc = manifest(vec![file("bc", 0o100644, 1)]);
    assert_ne!(
        key_of(copy("ab", false), Some(&m_c)),
        key_of(copy("a", false), Some(&m_bc))
    );

    let one = manifest(vec![file("a/b", 0o100644, 1), file("c", 0o100644, 1)]);
    let two = manifest(vec![file("a", 0o100644, 1), file("b/c", 0o100644, 1)]);
    assert_ne!(
        key_of(copy("x", false), Some(&one)),
        key_of(copy("x", false), Some(&two))
    );

    // A symlink target that looks like a file hash is still a symlink target.
    let link = manifest(vec![entry(
        "l",
        0o120000,
        EntryContent::SymlinkTarget(sha(1).to_string()),
    )]);
    let blob = manifest(vec![file("l", 0o100644, 1)]);
    assert_ne!(
        key_of(copy("x", false), Some(&link)),
        key_of(copy("x", false), Some(&blob))
    );
}

#[skuld::test]
fn rename_and_chmod_change_the_manifest_key() {
    let base = Chain::base();
    let key = |entries| {
        let m = manifest(entries);
        step_key(&base.ctx(), &base.root(), &base.copy, Some(&m)).unwrap()
    };
    let original = key(vec![file("a.txt", 0o100644, 1)]);
    assert_ne!(original, key(vec![file("b.txt", 0o100644, 1)]), "rename");
    assert_ne!(original, key(vec![file("a.txt", 0o100755, 1)]), "chmod +x");
    assert_ne!(original, key(vec![file("a.txt", 0o100644, 2)]), "content");
    let link = |target: &str| {
        key(vec![entry(
            "a.txt",
            0o120000,
            EntryContent::SymlinkTarget(target.into()),
        )])
    };
    assert_ne!(link("x"), link("y"), "symlink target");
    // Entry order does not matter: the constructor sorts.
    let forward = key(vec![file("a", 0o100644, 1), file("b", 0o100644, 2)]);
    let backward = key(vec![file("b", 0o100644, 2), file("a", 0o100644, 1)]);
    assert_eq!(forward, backward);
}

#[skuld::test]
fn manifest_constructor_rejects_bad_paths() {
    for path in [
        "", "/abs", "a//b", ".", "..", "a/../b", "a/./b", "a/", "a\\b", "a\0b", "../x", "x/..",
    ] {
        let error = CopyManifest::new(vec![file(path, 0o100644, 1)]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Recipe, "{path:?}");
    }
    for path in ["a", "a/b", "dir/файл with space.txt", "..a", "a..", ".hidden/.x"] {
        assert!(CopyManifest::new(vec![file(path, 0o100644, 1)]).is_ok(), "{path:?}");
    }
}

#[skuld::test]
fn manifest_rejects_duplicates_and_inconsistent_modes() {
    let duplicate = CopyManifest::new(vec![file("a", 0o100644, 1), file("a", 0o100755, 2)]);
    assert_eq!(duplicate.unwrap_err().kind(), ErrorKind::Recipe);
    let bad_mode = CopyManifest::new(vec![file("a", 0o100600, 1)]);
    assert_eq!(bad_mode.unwrap_err().kind(), ErrorKind::Recipe);
    let link_as_file = CopyManifest::new(vec![file("a", 0o120000, 1)]);
    assert_eq!(link_as_file.unwrap_err().kind(), ErrorKind::Recipe);
    let file_as_link = CopyManifest::new(vec![entry("a", 0o100644, EntryContent::SymlinkTarget("x".into()))]);
    assert_eq!(file_as_link.unwrap_err().kind(), ErrorKind::Recipe);
    assert!(CopyManifest::new(Vec::new()).is_ok());
}

// Recipes =============================================================================================================

const SHA_TEXT: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

fn keys_of(text: &str, origin: &Path) -> Vec<LayerKey> {
    let flat = Recipe::parse(text, origin).unwrap().flatten().unwrap();
    let ctx = KeyContext {
        arch: Arch::Amd64,
        disk: flat.resources.disk,
        guest_semantics_version: 1,
    };
    let content = RootContent::Sha256(SHA_TEXT.parse().unwrap());
    let manifests: Vec<Option<CopyManifest>> = flat
        .steps
        .iter()
        .map(|flat| match flat.step {
            Step::Copy { .. } => Some(manifest(vec![file("src/main.rs", 0o100644, 3)])),
            _ => None,
        })
        .collect();
    chain_keys(&flat, &ctx, &content, &[5; 32], &manifests).unwrap()
}

const PLAIN: &str = "from: ubuntu-24.04\nresources: { disk: 40GiB }\nsteps:\n  - run: |\n      echo a\n      echo b\n  - copy: { from: git-tracked, to: ~/p }\n    # trailing comment\n";
const COSMETIC: &str = "# A leading comment\n\nsteps:\n    -   copy:\n          to: \"~/p\"\n          from: git-tracked\n\n    -   run: |\n          echo a\n          echo b\n        # note\nresources:\n    disk: 40GiB\n\nfrom: \"ubuntu-24.04\"\n";

#[skuld::test]
fn recipe_text_cosmetics_do_not_change_the_key() {
    let origin = Path::new("x.devvm.yaml");
    // The cosmetic text lists the steps in a different order on purpose: step order IS meaningful.
    let reordered = keys_of(COSMETIC, origin);
    let plain = keys_of(PLAIN, origin);
    assert_ne!(plain, reordered, "step order must change the keys");

    let same_order = "\n# c\nfrom:   ubuntu-24.04   # base\n\nresources:\n    disk:   40GiB\nsteps:\n    -   run: |\n          echo a\n          echo b\n\n    -   copy:\n          to: \"~/p\"\n          from: git-tracked\n";
    assert_eq!(plain, keys_of(same_order, origin));
}

#[skuld::test]
fn crlf_and_bom_recipe_gives_the_same_key() {
    let origin = Path::new("x.devvm.yaml");
    let crlf = format!("\u{feff}{}", PLAIN.replace('\n', "\r\n"));
    assert_eq!(keys_of(PLAIN, origin), keys_of(&crlf, origin));
}

#[skuld::test]
fn child_disk_override_changes_every_layer_key_of_the_chain(#[fixture(unicode_home)] home: &Path) {
    std::fs::write(
        home.join("parent.devvm.yaml"),
        "from: ubuntu-24.04\nresources: { disk: 20GiB }\nsteps:\n  - run: one\n  - run: two\n",
    )
    .unwrap();
    let child = home.join("child.devvm.yaml");
    std::fs::write(
        &child,
        "from: parent.devvm.yaml\nresources: { disk: 40GiB }\nsteps:\n  - run: three\n",
    )
    .unwrap();

    let keys = |path: &Path| keys_of(&std::fs::read_to_string(path).unwrap(), path);
    let parent = keys(&home.join("parent.devvm.yaml"));
    let child = keys(&child);
    assert_eq!((parent.len(), child.len()), (3, 4));
    let parent: HashSet<_> = parent.into_iter().collect();
    assert!(
        child.iter().all(|key| !parent.contains(key)),
        "a layer key is shared across disk sizes"
    );
}

#[skuld::test]
fn layer_key_displays_and_parses_as_32_hex() {
    let key = Chain::base().last();
    let text = key.to_string();
    assert_eq!(text.len(), 32);
    assert!(text.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_eq!(text.parse::<LayerKey>().unwrap(), key);
    assert_eq!(text.to_uppercase().parse::<LayerKey>().unwrap(), key);
    for bad in ["", "abc", &text[..31], &format!("{text}0"), &format!("g{}", &text[1..])] {
        assert!(bad.parse::<LayerKey>().is_err(), "{bad:?}");
    }
}

#[skuld::test]
fn keys_are_send_and_sync() {
    fn assert_bounds<T: Send + Sync>() {}
    assert_bounds::<LayerKey>();
    assert_bounds::<CopyManifest>();
}

// Golden ==============================================================================================================

/// Pins the byte-level framing: this test must pass unchanged on every host OS, and changing the framing or any
/// hashed input is a new key scheme (bump `DOMAIN`) and an edit here.
#[skuld::test]
fn golden_key_for_a_fixed_recipe() {
    let text = "from: ubuntu-24.04\nresources: { disk: 40GiB }\nsteps:\n  - run: echo hello\n    shell: bash\n    elevated: true\n    cwd: /srv\n  - copy: { from: git-tracked, to: ~/proj, read-only: true }\n  - set: { uac: auto-approve }\n";
    let flat = Recipe::parse(text, Path::new("golden.devvm.yaml"))
        .unwrap()
        .flatten()
        .unwrap();
    let ctx = KeyContext {
        arch: Arch::Amd64,
        disk: flat.resources.disk,
        guest_semantics_version: 3,
    };
    let content = RootContent::Sha256(sha(0xab));
    let manifest = manifest(vec![
        file("src/main.rs", 0o100644, 0x01),
        file("run.sh", 0o100755, 0x02),
        entry("link", 0o120000, EntryContent::SymlinkTarget("src/main.rs".into())),
    ]);
    let keys = chain_keys(&flat, &ctx, &content, &[0xcd; 32], &[None, Some(manifest), None]).unwrap();
    let keys: Vec<String> = keys.iter().map(ToString::to_string).collect();
    assert_eq!(
        keys,
        [
            "00ba9f166afd810916d41e029a00b0bc",
            "94fcfa3c50b8d2a6d1d56b01aadebbcf",
            "d3f8588161c25c87b0330d97a274d72a",
            "4e7a141cfd7a26229edb4a98c341bb62",
        ]
    );
}
