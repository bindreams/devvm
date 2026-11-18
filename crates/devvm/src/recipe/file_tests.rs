use std::path::Path;

use crate::recipe::{CopyFrom, PerArch, Pin, ProductKey, Recipe, Setting, Shell, Source, Step, UacMode};
use crate::test_support::fixtures::unicode_home;
use crate::{Arch, ErrorKind, RunAs};

const SHA_A: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const SHA_B: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

fn pin(url: &str, sha: &str) -> Pin {
    Pin {
        url: url.to_owned(),
        sha256: sha.parse().unwrap(),
    }
}

fn parse(text: &str) -> Recipe {
    Recipe::parse(text, Path::new("proj/x.devvm.yaml")).unwrap()
}

fn parse_err(text: &str) -> crate::Error {
    Recipe::parse(text, Path::new("proj/x.devvm.yaml")).unwrap_err()
}

#[skuld::test]
fn each_from_variant_parses() {
    let dir = Path::new("proj");
    assert_eq!(
        parse("from: ./base.devvm.yaml").source,
        Source::Recipe(dir.join("./base.devvm.yaml"))
    );
    assert_eq!(
        parse("from: ubuntu-24.04").source,
        Source::Embedded("ubuntu-24.04".into())
    );
    assert_eq!(
        parse("from: { iso: ../isos/win.iso }").source,
        Source::Iso {
            path: dir.join("../isos/win.iso"),
            edition: None,
            product_key: None,
        }
    );

    let oci = format!("from: {{ oci: ghcr.io/x/macos:15, digest: 'sha256:{SHA_A}' }}");
    assert_eq!(
        parse(&oci).source,
        Source::Oci {
            reference: "ghcr.io/x/macos:15".into(),
            digest: format!("sha256:{SHA_A}")
        }
    );

    let one = format!("from: {{ url: 'https://h/a.img', sha256: {SHA_A} }}");
    assert_eq!(
        parse(&one).source,
        Source::DiskImage(PerArch::any(pin("https://h/a.img", SHA_A)))
    );

    let both = format!(
        "from:\n  amd64: {{ url: 'https://h/a-amd64.img', sha256: {SHA_A} }}\n  arm64: {{ url: 'https://h/a-arm64.img', sha256: {SHA_B} }}\n"
    );
    let Source::DiskImage(per_arch) = parse(&both).source else {
        panic!()
    };
    assert_eq!(per_arch.get(Arch::Amd64), Some(&pin("https://h/a-amd64.img", SHA_A)));
    assert_eq!(per_arch.get(Arch::Arm64), Some(&pin("https://h/a-arm64.img", SHA_B)));

    let only_arm = format!("from:\n  arm64: {{ url: 'https://h/a.img', sha256: {SHA_A} }}\n");
    let Source::DiskImage(per_arch) = parse(&only_arm).source else {
        panic!()
    };
    assert!(per_arch.get(Arch::Amd64).is_none() && per_arch.get(Arch::Arm64).is_some());
}

#[skuld::test]
fn relative_from_resolves_against_the_recipe_directory(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("a b").join("x.devvm.yaml");
    let recipe = Recipe::parse("from: ../base.devvm.yaml", &file).unwrap();
    assert_eq!(
        recipe.source,
        Source::Recipe(home.join("a b").join("../base.devvm.yaml"))
    );
    assert_eq!(recipe.dir, home.join("a b"));
    let recipe = Recipe::parse("from: { iso: win.iso }", &file).unwrap();
    assert_eq!(
        recipe.source,
        Source::Iso {
            path: home.join("a b").join("win.iso"),
            edition: None,
            product_key: None,
        }
    );
}

/// `Path` equality ignores `.` components, so these compare the text a person would see.
#[skuld::test]
fn an_iso_path_written_with_dot_slash_has_no_dot_in_it(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("a b").join("x.devvm.yaml");
    let Source::Iso { path, .. } = Recipe::parse("from: { iso: ./windows.iso }", &file).unwrap().source else {
        panic!()
    };
    assert_eq!(
        path.display().to_string(),
        home.join("a b").join("windows.iso").display().to_string()
    );
    let Source::Iso { path, .. } = Recipe::parse("from: { iso: ../iso/windows.iso }", &file)
        .unwrap()
        .source
    else {
        panic!()
    };
    assert_eq!(
        path.display().to_string(),
        home.join("a b")
            .join("..")
            .join("iso")
            .join("windows.iso")
            .display()
            .to_string()
    );
}

#[skuld::test]
fn a_recipe_named_by_a_bare_file_name_has_no_dot_in_its_paths() {
    for file in ["x.devvm.yaml", "./x.devvm.yaml"] {
        let recipe = Recipe::parse("from: { iso: ./windows.iso }", Path::new(file)).unwrap();
        let Source::Iso { path, .. } = recipe.source else {
            panic!()
        };
        assert_eq!(path.display().to_string(), "windows.iso", "recipe {file}");
        let recipe = Recipe::parse("from: ./base.devvm.yaml", Path::new(file)).unwrap();
        let Source::Recipe(path) = recipe.source else { panic!() };
        assert_eq!(path.display().to_string(), "base.devvm.yaml", "recipe {file}");
    }
}

#[skuld::test]
fn a_base_recipe_named_with_dot_slash_is_joined_without_it(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("a b").join("x.devvm.yaml");
    let Source::Recipe(path) = Recipe::parse("from: ./base.devvm.yaml", &file).unwrap().source else {
        panic!()
    };
    assert_eq!(
        path.display().to_string(),
        home.join("a b").join("base.devvm.yaml").display().to_string()
    );
}

#[skuld::test]
fn an_iso_takes_an_edition() {
    assert_eq!(
        parse("from: { iso: win.iso, edition: Windows 11 Pro }").source,
        Source::Iso {
            path: Path::new("proj").join("win.iso"),
            edition: Some("Windows 11 Pro".to_owned()),
            product_key: None,
        }
    );
    let error = parse_err("from: { iso: win.iso, edition: '  ' }");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("`edition`"), "{error}");
    // Only an ISO has editions.
    let error = parse_err(&format!("from: {{ url: x, sha256: {SHA_A}, edition: Pro }}"));
    assert_eq!(error.kind(), ErrorKind::Recipe);
}

const KEY: &str = "AB12C-DE34F-GH56J-KL78M-NP90Q";

#[skuld::test]
fn an_iso_takes_a_product_key() {
    assert_eq!(
        parse(&format!(
            "from: {{ iso: win.iso, edition: Windows 11 Pro, product-key: {KEY} }}"
        ))
        .source,
        Source::Iso {
            path: Path::new("proj").join("win.iso"),
            edition: Some("Windows 11 Pro".to_owned()),
            product_key: Some(ProductKey::parse(KEY).unwrap()),
        }
    );
    let Source::Iso { product_key, .. } = parse(&format!("from: {{ iso: win.iso, product-key: {KEY} }}")).source else {
        panic!()
    };
    assert_eq!(product_key, Some(ProductKey::parse(KEY).unwrap()));
}

#[skuld::test]
fn a_product_key_of_the_wrong_shape_is_refused_without_being_printed() {
    for bad in [
        "AB12C-DE34F-GH56J-KL78M",
        "AB12C-DE34F-GH56J-KL78M-NP90Q-XY12Z",
        "AB12C-DE34F-GH56J-KL78M-NP90",
        "AB12C-DE34F-GH56J-KL78M-NP90QR",
        "AB12CDE34FGH56JKL78MNP90Q",
        "AB12C-DE34F-GH56J-KL78M-NP9 Q",
        "AB12C-DE34F-GH56J-KL78M-NP9ÉQ",
        "AB12C_DE34F_GH56J_KL78M_NP90Q",
        "",
    ] {
        let error = parse_err(&format!("from: {{ iso: win.iso, product-key: \"{bad}\" }}"));
        assert_eq!(error.kind(), ErrorKind::Recipe, "{bad}");
        assert_eq!(
            error.message(),
            "invalid recipe proj/x.devvm.yaml: `product-key` must be five groups of five letters and digits, XXXXX-XXXXX-XXXXX-XXXXX-XXXXX",
            "{bad}"
        );
    }
    let error = parse_err(&format!("from: {{ url: x, sha256: {SHA_A}, product-key: {KEY} }}"));
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(!error.to_string().contains(KEY), "{error}");
}

#[skuld::test]
fn a_product_key_is_kept_in_uppercase() {
    // Windows keys are case-insensitive: the answer file and the layer key see one spelling.
    let lower = ProductKey::parse("ab12c-de34f-gh56j-kl78m-np90q").unwrap();
    assert_eq!(lower.expose(), KEY);
    assert_eq!(lower, ProductKey::parse(KEY).unwrap());
    let Source::Iso { product_key, .. } =
        parse("from: { iso: win.iso, product-key: ab12c-de34f-gh56j-kl78m-np90q }").source
    else {
        panic!()
    };
    assert_eq!(product_key.unwrap().expose(), KEY);
}

#[skuld::test]
fn the_product_key_is_in_no_debug_text_of_a_recipe() {
    let recipe = parse(&format!("from: {{ iso: win.iso, product-key: {KEY} }}"));
    for text in [
        format!("{recipe:?}"),
        format!("{:?}", recipe.source),
        format!("{:#?}", recipe.source),
    ] {
        assert!(!text.contains(KEY), "{text}");
    }
    let key = ProductKey::parse(KEY).unwrap();
    assert!(!format!("{key:?}").contains(KEY));
}

#[skuld::test]
fn from_file_reads_the_file(#[fixture(unicode_home)] home: &Path) {
    let file = home.join("x.devvm.yaml");
    std::fs::write(&file, "from: ubuntu-24.04\n").unwrap();
    let recipe = Recipe::from_file(&file).unwrap();
    assert_eq!(recipe.name, "x");
    let error = Recipe::from_file(&home.join("nope.devvm.yaml")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.message().contains("nope.devvm.yaml"), "{error}");
}

#[skuld::test]
fn every_step_variant_parses_with_defaults() {
    let recipe = parse(
        r#"
from: ubuntu-24.04
steps:
  - run: echo hi
  - run: apt-get update
    shell: bash
    elevated: true
    cwd: /tmp
  - run: dir
    shell: cmd
  - copy: { from: git-tracked, to: ~/proj }
  - copy: { from: git-tracked, to: 'C:\proj', read-only: true }
  - set: { uac: auto-approve }
"#,
    );
    let run = |command: &str, shell, run_as, cwd: Option<&str>| Step::Run {
        command: command.into(),
        shell,
        run_as,
        cwd: cwd.map(Into::into),
    };
    assert_eq!(
        recipe.steps,
        vec![
            run("echo hi", None, RunAs::User, None),
            run("apt-get update", Some(Shell::Bash), RunAs::Elevated, Some("/tmp")),
            run("dir", Some(Shell::Cmd), RunAs::User, None),
            Step::Copy {
                from: CopyFrom::GitTracked,
                to: "~/proj".into(),
                read_only: false
            },
            Step::Copy {
                from: CopyFrom::GitTracked,
                to: r"C:\proj".into(),
                read_only: true
            },
            Step::Set(Setting::Uac(UacMode::AutoApprove)),
        ]
    );
    for (name, shell) in [("sh", Shell::Sh), ("pwsh", Shell::Pwsh)] {
        let recipe = parse(&format!("from: u\nsteps:\n  - run: x\n    shell: {name}\n"));
        assert!(matches!(&recipe.steps[0], Step::Run { shell: Some(s), .. } if *s == shell));
    }
}

#[skuld::test]
fn name_defaults_to_the_file_stem() {
    assert_eq!(parse("from: u").name, "x");
    assert_eq!(parse("from: u\nname: custom").name, "custom");
    let recipe = Recipe::parse("from: u", Path::new("dir/cosca-linux.devvm.yaml")).unwrap();
    assert_eq!(recipe.name, "cosca-linux");
}

#[skuld::test]
fn resources_parse_binary_sizes() {
    let recipe = parse("from: u\nresources: { cpus: 4, memory: 8GiB, disk: 40GiB }");
    assert_eq!(recipe.resources.cpus, Some(4));
    assert_eq!(recipe.resources.memory, Some(8 * 1024 * 1024 * 1024));
    assert_eq!(recipe.resources.disk, Some(40 * 1024 * 1024 * 1024));
    let recipe = parse("from: u");
    assert_eq!(
        (recipe.resources.cpus, recipe.resources.memory, recipe.resources.disk),
        (None, None, None)
    );
    let error = parse_err("from: u\nresources: { disk: lots }");
    assert_eq!(error.kind(), ErrorKind::Recipe);
}

#[skuld::test]
fn bom_crlf_and_tabs_in_block_scalars_parse() {
    let lf = "from: ubuntu-24.04\nsteps:\n  - run: |\n      echo a\n      \techo b\n      echo c\n  - copy: { from: git-tracked, to: ~/p }\n";
    let crlf = format!("\u{feff}{}", lf.replace('\n', "\r\n"));
    assert_eq!(parse(lf), parse(&crlf));
    let Step::Run { command, .. } = &parse(&crlf).steps[0] else {
        panic!()
    };
    assert_eq!(command, "echo a\n\techo b\necho c\n");
}

#[skuld::test]
fn unknown_field_names_it_and_the_file() {
    let error = parse_err("from: u\nfrobnicate: 1");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("frobnicate"), "{error}");
    assert!(error.message().contains("x.devvm.yaml"), "{error}");
    let error = parse_err("from: u\nsteps:\n  - run: x\n    shel: bash");
    assert!(error.message().contains("shel"), "{error}");
    let error = parse_err("from: u\nresources: { cpu: 2 }");
    assert!(error.message().contains("cpu"), "{error}");
}

#[skuld::test]
fn missing_from_is_an_error() {
    let error = parse_err("steps: []");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("from"), "{error}");
}

#[skuld::test]
fn duplicate_key_is_an_error() {
    let error = parse_err("from: a\nfrom: b");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("x.devvm.yaml"), "{error}");
}

#[skuld::test]
fn step_with_two_actions_is_an_error() {
    let error = parse_err("from: u\nsteps:\n  - run: x\n    set: { uac: auto-approve }");
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(error.message().contains("exactly one"), "{error}");
    let error = parse_err("from: u\nsteps:\n  - cwd: /x");
    assert!(error.message().contains("exactly one"), "{error}");
}

#[skuld::test]
fn modifier_on_the_wrong_action_is_an_error() {
    for step in [
        "- copy: { from: git-tracked, to: x }\n    shell: bash",
        "- copy: { from: git-tracked, to: x }\n    elevated: true",
        "- set: { uac: auto-approve }\n    cwd: /x",
    ] {
        let error = parse_err(&format!("from: u\nsteps:\n  {step}"));
        assert_eq!(error.kind(), ErrorKind::Recipe, "{step}");
        assert!(error.message().contains("only valid on `run`"), "{step}: {error}");
    }
}

#[skuld::test]
fn warm_is_rejected_naming_it() {
    for value in ["true", "false"] {
        let error = parse_err(&format!("from: u\nwarm: {value}"));
        assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
        assert!(error.message().contains("`warm`"), "{error}");
        assert!(error.message().contains("not supported in devvm 0.1.0"), "{error}");
        assert!(error.message().contains("x.devvm.yaml"), "{error}");
    }
}

#[skuld::test]
fn arch_list_is_rejected_naming_it() {
    let error = parse_err("from: u\narch: [amd64, arm64]");
    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    assert!(
        error.message().contains("`arch`") && error.message().contains("multi-arch"),
        "{error}"
    );
    assert!(error.message().contains("not supported in devvm 0.1.0"), "{error}");
    assert!(error.message().contains("x.devvm.yaml"), "{error}");
}

#[skuld::test]
fn scalar_arch_is_an_unknown_field_error() {
    let error = parse_err("from: u\narch: amd64");
    assert_eq!(error.kind(), ErrorKind::Recipe, "{error}");
    assert!(error.message().contains("arch"), "{error}");
}

#[skuld::test]
fn out_of_scope_keys_win_over_other_errors() {
    // The file is also missing `from`; the unsupported feature is still what gets named.
    let error = parse_err("warm: true");
    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
}

#[skuld::test]
fn oci_digest_is_stored_lowercase() {
    let upper = SHA_B.to_ascii_uppercase();
    let oci = format!("from: {{ oci: ghcr.io/x/macos:15, digest: 'sha256:{upper}' }}");
    assert_eq!(
        parse(&oci).source,
        Source::Oci {
            reference: "ghcr.io/x/macos:15".into(),
            digest: format!("sha256:{SHA_B}")
        }
    );
}

#[skuld::test]
fn bad_values_are_errors() {
    let cases = [
        "from: { url: 'https://h/a', sha256: abc }".to_owned(),
        "from: { oci: r, digest: 'sha256:abc' }".to_owned(),
        format!("from: {{ oci: r, digest: '{SHA_A}' }}"),
        "from: { url: 'https://h/a' }".to_owned(),
        "from: {}".to_owned(),
        "from: { iso: a, oci: b, digest: c }".to_owned(),
        "from: u\nsteps:\n  - copy: { from: working-tree, to: x }".to_owned(),
        "from: u\nsteps:\n  - set: { uac: deny }".to_owned(),
        "from: u\nsteps:\n  - set: {}".to_owned(),
        "from: u\nsteps:\n  - run: x\n    shell: fish".to_owned(),
        "from: u\nsteps:\n  - run: [a, b]".to_owned(),
        "from: 5".to_owned(),
    ];
    for text in cases {
        let error = parse_err(&text);
        assert_eq!(error.kind(), ErrorKind::Recipe, "{text}: {error}");
        assert!(error.message().contains("x.devvm.yaml"), "{text}: {error}");
    }
}
