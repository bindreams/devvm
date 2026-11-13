//! `Engine::build` over the fake backend, whose machines are served by the test SSH server. The tests that launch a
//! machine need a frozen spawn, which only Windows has, and run in `build_windows_tests.rs`; these never launch one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::access::keys::InstanceKeys;
use crate::backend::Backend;
use crate::backend::fake::FakeBackend;
use crate::engine::build::{Lines, extract_command, guest_destination, root_content};
use crate::engine::engine_tests::{engine_with, is_there, memory};
use crate::engine::{Engine, ListFilter};
use crate::guest::fake::{FakeBaseBuilder, FakeGuests, ServedGuests};
use crate::guest::{BaseBuilder, GuestOs, Guests};
use crate::host::symlink_metadata_if_present;
use crate::recipe::Recipe;
use crate::recipe::hash::{KeyContext, LayerKey, chain_keys};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::sshd::{SshdOptions, TestSshd};
use crate::{Arch, BuildOptions, Dirs, ErrorKind, GuestKind};

// Fixtures ============================================================================================================

/// An engine whose guests and machines are served by one SSH server, whose guest user's home is `/home/user`.
pub(super) struct Fixture {
    pub(super) engine: Arc<Engine>,
    pub(super) fake: Arc<FakeBackend>,
    pub(super) server: Arc<TestSshd>,
    pub(super) guests: Arc<ServedGuests>,
    pub(super) progress: Arc<MemoryProgress>,
    pub(super) dirs: Dirs,
}

pub(super) fn fixture(home: &Path) -> Fixture {
    fixture_over(home, FakeBackend::new(&TestHost))
}

fn fixture_over(home: &Path, backend: FakeBackend) -> Fixture {
    let dirs = Dirs::under(home);
    let server = Arc::new(TestSshd::start(SshdOptions {
        client_public: InstanceKeys::generate().unwrap().client_public,
        home: Some("/home/user".to_owned()),
        ..SshdOptions::default()
    }));
    // The guest's `/tmp`, which the archive of a copy step is uploaded to.
    std::fs::create_dir_all(server.root().join("tmp")).unwrap();
    let fake = Arc::new(backend.with_ssh_endpoint(server.addr()));
    let progress = memory();
    let guests = Arc::new(ServedGuests::new(server.clone()));
    let engine = engine_with(
        dirs.clone(),
        &TestHost,
        Some(fake.clone() as Arc<dyn Backend>),
        guests.clone(),
        progress.clone(),
    );
    Fixture {
        engine,
        fake,
        server,
        guests,
        progress,
        dirs,
    }
}

/// A recipe file at `dir/r.devvm.yaml` that starts from the embedded Ubuntu base. `top` is more top-level keys, and
/// each of `steps` is one step's YAML.
pub(super) fn recipe_in(dir: &Path, top: &str, steps: &[&str]) -> Recipe {
    let steps: String = steps.iter().map(|step| format!("  - {step}\n")).collect();
    Recipe::parse(
        &format!("from: ubuntu-24.04\n{top}steps:\n{steps}"),
        &dir.join("r.devvm.yaml"),
    )
    .unwrap()
}

pub(super) fn recipe(home: &Path, steps: &[&str]) -> Recipe {
    recipe_in(home, "", steps)
}

/// The keys of the layers `recipe` builds, root first, when it has no `copy` step.
pub(super) fn keys_of(recipe: &Recipe) -> Vec<LayerKey> {
    let flat = recipe.flatten().unwrap();
    let (root, _) = root_content(&flat.root, Arch::host(), &|_| unreachable!("no ISO here")).unwrap();
    let ctx = KeyContext {
        arch: Arch::host(),
        disk: flat.resources.disk,
        guest_semantics_version: FakeGuests.os(GuestKind::Linux).unwrap().semantics_version(),
    };
    let manifests = vec![None; flat.steps.len()];
    chain_keys(&flat, &ctx, &root, &FakeBaseBuilder.definition_hash(), &manifests).unwrap()
}

/// The commands the server was given that a recipe step ran, in order: the guest's `bash -c` of a `run` step.
pub(super) fn ran(fx: &Fixture) -> Vec<String> {
    fx.server
        .commands()
        .into_iter()
        .filter_map(|command| command.strip_prefix("bash -c ").map(str::to_owned))
        .collect()
}

pub(super) fn no_instance_or_build_is_left(fx: &Fixture) {
    assert!(
        fx.engine.list(ListFilter::All).unwrap().is_empty(),
        "an instance is left"
    );
    assert!(
        fx.engine.registry().locks().build_locks().unwrap().is_empty(),
        "a build lock is left"
    );
    let builds: Vec<PathBuf> = if symlink_metadata_if_present(&fx.dirs.builds()).unwrap().is_none() {
        Vec::new()
    } else {
        std::fs::read_dir(fx.dirs.builds())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect()
    };
    assert!(builds.is_empty(), "a build directory is left: {builds:?}");
}

// Steps ===============================================================================================================

/// A one-step build over `backend` fails with an error that names the step, `context` (the phase it was in) and
/// `cause`; the step's layer is not in the store and no instance or build directory is left.
pub(super) fn assert_a_failed_phase_commits_nothing(
    home: &Path,
    backend: FakeBackend,
    context: &str,
    cause: &str,
) -> Fixture {
    let fx = fixture_over(home, backend);
    let recipe = recipe(home, &["run: echo one"]);
    let keys = keys_of(&recipe);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    let message = error.message();
    assert!(message.contains("step 1 of 1"), "{message}");
    assert!(message.contains(&format!("({context})")), "{message}");
    assert!(message.contains(cause), "{message}");
    assert!(
        fx.engine.store().unwrap().lookup(&keys[1]).unwrap().is_none(),
        "the step's layer was published"
    );
    no_instance_or_build_is_left(&fx);
    fx
}

#[skuld::test]
fn a_guest_that_cannot_start_names_the_step_and_commits_nothing(#[fixture(unicode_home)] home: &Path) {
    assert_a_failed_phase_commits_nothing(
        home,
        FakeBackend::new(&TestHost).with_failing_launch(),
        "starting the step's guest",
        "the fake backend cannot launch",
    );
}

// Pure parts ==========================================================================================================

#[skuld::test]
fn an_iso_root_is_keyed_by_content(#[fixture(unicode_home)] home: &Path) {
    use crate::iso::IsoMemo;
    use crate::recipe::Source;
    let memo = IsoMemo::new(&Dirs::under(home));
    let progress = MemoryProgress::default();
    let (one, two, other) = (home.join("one.iso"), home.join("two.iso"), home.join("other.iso"));
    std::fs::write(&one, b"same bytes").unwrap();
    std::fs::write(&two, b"same bytes").unwrap();
    std::fs::write(&other, b"other bytes").unwrap();
    let key = |path: &Path, edition: Option<&str>| {
        let source = Source::Iso {
            path: path.to_owned(),
            edition: edition.map(Into::into),
        };
        let (content, _) = root_content(&source, Arch::host(), &|iso| memo.sha256(&TestHost, iso, &progress)).unwrap();
        let ctx = KeyContext {
            arch: Arch::host(),
            disk: None,
            guest_semantics_version: 1,
        };
        crate::recipe::hash::root_key(&ctx, &content, &[0; 32])
    };
    assert_eq!(key(&one, None), key(&two, None), "two paths, equal bytes, one key");
    assert_ne!(key(&one, None), key(&other, None));
    assert_ne!(key(&one, None), key(&one, Some("Windows 11 Pro")));
}

fn linux() -> Arc<dyn GuestOs> {
    FakeGuests.os(GuestKind::Linux).unwrap()
}

#[skuld::test]
fn a_destination_with_no_root_is_made_absolute_below_the_guest_home() {
    let guest = linux();
    for (expanded, resolved) in [
        ("/home/user/cosca", "/home/user/cosca"),
        ("/home/user", "/home/user"),
        ("rel/x", "/home/user/rel/x"),
        ("a b", "/home/user/a b"),
        ("/abs", "/abs"),
    ] {
        assert_eq!(
            guest_destination(&*guest, expanded, "/home/user").unwrap(),
            resolved,
            "{expanded}"
        );
    }
    assert_eq!(guest_destination(&*guest, "rel", "/").unwrap(), "/rel", "a home of `/`");
    assert_eq!(
        guest_destination(&*guest, "rel", "/home/user/").unwrap(),
        "/home/user/rel"
    );
}

#[skuld::test]
fn a_copy_step_whose_files_cannot_be_snapshotted_names_the_step_and_its_destination(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    let plain = home.join("not a repo");
    std::fs::create_dir_all(&plain).unwrap();
    let recipe = recipe_in(
        &plain,
        "",
        &["run: echo one", "copy: { from: git-tracked, to: /srv/x }"],
    );

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    assert_eq!(error.kind(), ErrorKind::Recipe, "{error}");
    let message = error.message();
    assert!(
        message.contains("step 2 of 2 (snapshotting the files of copy to /srv/x)"),
        "{message}"
    );
    assert!(ran(&fx).is_empty(), "nothing ran: the snapshot is taken first");
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn the_extraction_command_takes_its_paths_as_arguments_and_removes_the_archive_whatever_happened() {
    let guest = linux();

    let command = extract_command(&*guest, "/home/user/a b; rm -rf /", "/tmp/devvm-copy-x.tar").unwrap();

    let [shell, flag, script, name, destination, archive] = command.argv.as_slice() else {
        panic!("{:?}", command.argv);
    };
    assert_eq!((shell.as_str(), flag.as_str()), ("sh", "-c"));
    assert_eq!(name, "devvm-copy");
    // The paths are arguments the script reads, never text in it.
    assert_eq!(destination, "/home/user/a b; rm -rf /");
    assert_eq!(archive, "/tmp/devvm-copy-x.tar");
    assert!(
        !script.contains("devvm-copy-x") && !script.contains("rm -rf /"),
        "{script}"
    );
    // Extract, then remove the archive whatever the extraction did, then end with the extraction's status.
    let at = |needle: &str| {
        script
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is not in {script}"))
    };
    assert!(
        at("tar -xf") < at("rm -f -- \"$2\"") && at("rm -f -- \"$2\"") < at("exit $status"),
        "{script}"
    );
    assert!(script.contains("status=$?;"), "{script}");
    assert!(
        !script.contains("&& rm"),
        "a failed extraction must still remove the archive: {script}"
    );
}

#[skuld::test]
fn set_uac_on_linux_is_a_recipe_error(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_with(Dirs::under(home), &TestHost, None, Arc::new(FakeGuests), memory());
    let recipe = recipe(home, &["run: echo one", "set: { uac: auto-approve }"]);

    let error = engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    assert_eq!(error.kind(), ErrorKind::Recipe, "{error}");
    assert!(
        error.message().contains("step 2") && error.message().contains("set.uac"),
        "{error}"
    );
    assert!(error.message().contains("linux"), "{error}");
}

#[skuld::test]
fn an_engine_without_a_backend_cannot_build_and_makes_nothing(#[fixture(unicode_home)] home: &Path) {
    let engine = engine_with(Dirs::under(home), &TestHost, None, Arc::new(FakeGuests), memory());

    let error = engine
        .build(&recipe(home, &["run: echo one"]), &BuildOptions::default())
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    assert!(!is_there(&Dirs::under(home).state) && !is_there(&Dirs::under(home).data));
}

/// What `Lines` reports for `chunks` written in turn, then dropped.
fn lines_reported(chunks: &[&[u8]]) -> Vec<String> {
    let progress = MemoryProgress::default();
    let mut lines = Lines::new(&progress);
    for chunk in chunks {
        lines.write_all(chunk).unwrap();
    }
    drop(lines);
    progress.messages()
}

#[skuld::test]
fn a_carriage_return_ends_a_line_of_output_like_a_line_feed() {
    // Progress output redraws one line with `\r`: each redraw is a line.
    assert_eq!(
        lines_reported(&[b"10%\r20%\r30%\ndone\n"]),
        ["10%", "20%", "30%", "done"]
    );
    assert_eq!(
        lines_reported(&[b"10%\r20%"]),
        ["10%", "20%"],
        "the rest is reported at the end"
    );
}

#[skuld::test]
fn a_carriage_return_and_line_feed_are_one_line_end_wherever_the_writes_split_them() {
    assert_eq!(lines_reported(&[b"a\r\nb\r\n"]), ["a", "b"]);
    assert_eq!(lines_reported(&[b"a\r", b"\nb\n"]), ["a", "b"]);
    assert_eq!(
        lines_reported(&[b"a\n\nb\n"]),
        ["a", "", "b"],
        "an empty line is a line"
    );
}

#[skuld::test]
fn a_carriage_return_with_nothing_pending_reports_no_line() {
    // A redraw that starts with `\r`, and a doubled `\r`, are not blank lines.
    assert_eq!(lines_reported(&[b"a\n\r10%\r100%\n"]), ["a", "10%", "100%"]);
    assert_eq!(lines_reported(&[b"x\r\r\n"]), ["x"]);
    assert_eq!(
        lines_reported(&[b"x\r", b"\r", b"\n"]),
        ["x"],
        "wherever the writes split them"
    );
}
