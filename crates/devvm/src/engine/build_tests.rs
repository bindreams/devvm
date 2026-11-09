//! `Engine::build` over the fake backend, whose machines are served by the test SSH server. The fake machines are
//! processes, and the `copy` tests run `git`, so the tests that build run in the deputy's runner and CI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::access::keys::InstanceKeys;
use crate::backend::Backend;
use crate::backend::fake::FakeBackend;
use crate::copy::copy_tests::{add_all, new_repo, put};
use crate::engine::build::{extract_command, guest_destination, root_content};
use crate::engine::engine_tests::{engine_with, is_there, memory};
use crate::engine::{Engine, ListFilter};
use crate::guest::fake::{FakeBaseBuilder, FakeGuests, ServedGuests};
use crate::guest::{BaseBuilder, GuestOs, Guests};
use crate::recipe::Recipe;
use crate::recipe::hash::{KeyContext, LayerKey, chain_keys};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::sshd::{SshdOptions, TestSshd};
use crate::{Arch, BuildOptions, Dirs, ErrorKind, GuestKind, Image};

// Fixtures ============================================================================================================

/// An engine whose guests and machines are served by one SSH server, whose guest user's home is `/home/user`.
struct Fixture {
    engine: Arc<Engine>,
    fake: Arc<FakeBackend>,
    server: Arc<TestSshd>,
    progress: Arc<MemoryProgress>,
    dirs: Dirs,
}

fn fixture(home: &Path) -> Fixture {
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
    let engine = engine_with(
        dirs.clone(),
        &TestHost,
        Some(fake.clone() as Arc<dyn Backend>),
        Arc::new(ServedGuests::new(server.clone())),
        progress.clone(),
    );
    Fixture {
        engine,
        fake,
        server,
        progress,
        dirs,
    }
}

/// A recipe file at `dir/r.devvm.yaml` that starts from the embedded Ubuntu base. `top` is more top-level keys, and
/// each of `steps` is one step's YAML.
fn recipe_in(dir: &Path, top: &str, steps: &[&str]) -> Recipe {
    let steps: String = steps.iter().map(|step| format!("  - {step}\n")).collect();
    Recipe::parse(
        &format!("from: ubuntu-24.04\n{top}steps:\n{steps}"),
        &dir.join("r.devvm.yaml"),
    )
    .unwrap()
}

fn recipe(home: &Path, steps: &[&str]) -> Recipe {
    recipe_in(home, "", steps)
}

/// The keys of the layers `recipe` builds, root first, when it has no `copy` step.
fn keys_of(recipe: &Recipe) -> Vec<LayerKey> {
    let flat = recipe.flatten().unwrap();
    let (root, _) = root_content(&flat.root, Arch::host()).unwrap();
    let ctx = KeyContext {
        arch: Arch::host(),
        disk: flat.resources.disk,
        guest_semantics_version: FakeGuests.os(GuestKind::Linux).unwrap().semantics_version(),
    };
    let manifests = vec![None; flat.steps.len()];
    chain_keys(&flat, &ctx, &root, &FakeBaseBuilder.definition_hash(), &manifests).unwrap()
}

fn layer_keys(image: &Image) -> Vec<LayerKey> {
    image.chain.refs().iter().map(|layer| layer.key).collect()
}

/// The commands the server was given that a recipe step ran, in order: the guest's `bash -c` of a `run` step.
fn ran(fx: &Fixture) -> Vec<String> {
    fx.server
        .commands()
        .into_iter()
        .filter_map(|command| command.strip_prefix("bash -c ").map(str::to_owned))
        .collect()
}

/// The commands of `copy` steps: the extraction of an uploaded archive.
fn extractions(fx: &Fixture) -> Vec<String> {
    fx.server
        .commands()
        .into_iter()
        .filter(|command| command.starts_with("sh -c ") && command.contains("tar -xf"))
        .collect()
}

fn no_instance_or_build_is_left(fx: &Fixture) {
    assert!(
        fx.engine.list(ListFilter::All).unwrap().is_empty(),
        "an instance is left"
    );
    assert!(
        fx.engine.registry().locks().build_locks().unwrap().is_empty(),
        "a build lock is left"
    );
    let builds: Vec<PathBuf> = match std::fs::read_dir(fx.dirs.builds()) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => panic!("cannot list the builds directory: {e}"),
    };
    assert!(builds.is_empty(), "a build directory is left: {builds:?}");
}

// Steps ===============================================================================================================

#[skuld::test]
fn build_runs_each_step_once_in_order_and_publishes_one_layer_per_step(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let recipe = recipe(home, &["run: echo one", "run: echo two", "run: echo three"]);

    let image = fx.engine.build(&recipe, &BuildOptions::default()).unwrap();

    assert_eq!(ran(&fx), ["echo one", "echo two", "echo three"]);
    assert_eq!(layer_keys(&image), keys_of(&recipe), "the base and one layer per step");
    let messages = fx.progress.messages();
    for output in ["one", "two", "three"] {
        assert!(messages.iter().any(|message| message == output), "{messages:?}");
    }

    // Every layer exists now, so building again runs nothing.
    let again = fx.engine.build(&recipe, &BuildOptions::default()).unwrap();
    assert_eq!(ran(&fx).len(), 3, "a second build ran a step");
    assert_eq!(again.chain.refs(), image.chain.refs());
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_failing_run_step_names_its_index_command_and_status_and_publishes_nothing(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let recipe = recipe(home, &["run: echo one", "run: exit 3", "run: echo three"]);
    let keys = keys_of(&recipe);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    assert_eq!(error.kind(), ErrorKind::Recipe, "{error}");
    let message = error.message();
    assert!(message.contains("step 2 of 3"), "{message}");
    assert!(message.contains("`exit 3`"), "{message}");
    assert!(message.contains("exit code 3"), "{message}");
    assert_eq!(ran(&fx), ["echo one", "exit 3"], "the steps after it never ran");
    let store = fx.engine.store().unwrap();
    assert!(store.lookup(&keys[1]).unwrap().is_some(), "the step before it is kept");
    assert!(store.lookup(&keys[2]).unwrap().is_none(), "nothing is published for it");
    assert!(store.lookup(&keys[3]).unwrap().is_none());
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_forced_shutdown_never_commits(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture_over(home, FakeBackend::new(&TestHost).with_failing_shutdown());
    let recipe = recipe(home, &["run: echo one"]);
    let keys = keys_of(&recipe);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    assert!(error.message().contains("does not power down"), "{error}");
    assert!(
        fx.engine.store().unwrap().lookup(&keys[1]).unwrap().is_none(),
        "a disk that was never shut down is not a layer"
    );
    assert_eq!(fx.fake.launches(), 1);
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_guest_that_was_down_before_the_shutdown_request_commits_nothing(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture_over(home, FakeBackend::new(&TestHost).with_vm_dying_before_shutdown());
    let recipe = recipe(home, &["run: echo one"]);
    let keys = keys_of(&recipe);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("step 1 of 1"), "{error}");
    assert!(
        fx.engine.store().unwrap().lookup(&keys[1]).unwrap().is_none(),
        "the disk of a guest that went down by itself is not a layer"
    );
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_transient_build_instance_is_removed_after_success_and_after_failure(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);

    fx.engine
        .build(&recipe(home, &["run: echo one"]), &BuildOptions::default())
        .unwrap();
    assert_eq!(fx.fake.launches(), 1);
    assert!(fx.engine.list(ListFilter::All).unwrap().is_empty());

    fx.engine
        .build(
            &recipe(home, &["run: echo one", "run: exit 1"]),
            &BuildOptions::default(),
        )
        .err()
        .unwrap();
    assert_eq!(fx.fake.launches(), 2, "the first step is cached, the second ran");
    assert!(fx.engine.list(ListFilter::All).unwrap().is_empty());
    // The instances' directories are gone, not only their names.
    let left: Vec<_> = std::fs::read_dir(fx.dirs.instances())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[skuld::test]
fn a_finished_or_failed_build_leaves_no_build_directory_and_no_lock_file(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"a");
    add_all(&repo);
    let steps = ["copy: { from: git-tracked, to: \"~/p\" }"];

    fx.engine
        .build(&recipe_in(&repo, "", &steps), &BuildOptions::default())
        .unwrap();
    no_instance_or_build_is_left(&fx);

    // The snapshot is made before any step runs; this build fails at its step.
    let failing = ["copy: { from: git-tracked, to: \"~root/p\" }"];
    fx.engine
        .build(&recipe_in(&repo, "", &failing), &BuildOptions::default())
        .err()
        .unwrap();
    no_instance_or_build_is_left(&fx);
}

// Resources ===========================================================================================================

#[skuld::test]
fn every_step_instance_gets_the_recipes_disk_unchanged_and_a_bigger_disk_shares_no_layer(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    let steps = ["run: echo one", "run: echo two"];

    let default = fx
        .engine
        .build(&recipe(home, &steps), &BuildOptions::default())
        .unwrap();
    assert_eq!(fx.fake.disk_sizes(), [None, None]);

    let bigger = fx
        .engine
        .build(
            &recipe_in(home, "resources:\n  disk: 20GiB\n", &steps),
            &BuildOptions::default(),
        )
        .unwrap();
    assert_eq!(fx.fake.disk_sizes()[2..], [Some(20 << 30), Some(20 << 30)]);
    let (default, bigger) = (layer_keys(&default), layer_keys(&bigger));
    assert!(
        default.iter().all(|key| !bigger.contains(key)),
        "the disk size is in every key: {default:?} {bigger:?}"
    );
}

// Copy steps ==========================================================================================================

#[skuld::test]
fn a_copy_step_issues_one_command_that_extracts_then_removes_the_archive_outside_the_destination(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"tracked");
    put(&repo, "dir/b.txt", b"also tracked");
    add_all(&repo);

    fx.engine
        .build(
            &recipe_in(&repo, "", &["copy: { from: git-tracked, to: \"~/p q\" }"]),
            &BuildOptions::default(),
        )
        .unwrap();

    let commands = extractions(&fx);
    assert_eq!(commands.len(), 1, "{commands:?}");
    let (_, arguments) = commands[0]
        .split_once(" devvm-copy ")
        .expect("the paths follow the script");
    let (destination, archive) = arguments.split_once(" /tmp/").expect("the archive is in /tmp");
    assert_eq!(destination, "/home/user/p q");
    let name = archive
        .strip_prefix("devvm-copy-")
        .and_then(|rest| rest.strip_suffix(".tar"))
        .unwrap();
    assert!(crate::random::is_hex(name, 16), "{archive}");
    // The tar the guest was given holds the tracked files.
    let uploaded = std::fs::File::open(fx.server.root().join("tmp").join(archive)).unwrap();
    let mut names: Vec<String> = tar::Archive::new(uploaded)
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().display().to_string())
        .collect();
    names.sort();
    assert!(
        names.contains(&"a.txt".to_owned()) && names.contains(&"dir/b.txt".to_owned()),
        "{names:?}"
    );
}

#[skuld::test]
fn two_copy_steps_over_one_destination_overwrite_in_order(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"tracked");
    add_all(&repo);
    let step = "copy: { from: git-tracked, to: \"~/p\" }";

    let image = fx
        .engine
        .build(
            &recipe_in(&repo, "", &[step, "run: echo between", step]),
            &BuildOptions::default(),
        )
        .unwrap();

    let commands = fx.server.commands();
    let position = |needle: &str| commands.iter().position(|command| command.contains(needle)).unwrap();
    let extracting: Vec<usize> = commands
        .iter()
        .enumerate()
        .filter(|(_, command)| command.contains("tar -xf"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(extracting.len(), 2);
    assert!(extracting[0] < position("echo between") && position("echo between") < extracting[1]);
    let archives: Vec<&str> = extracting
        .iter()
        .map(|index| commands[*index].rsplit_once(' ').unwrap().1)
        .collect();
    assert_ne!(archives[0], archives[1], "each step uploads an archive of its own");
    assert_eq!(image.chain.layers().len(), 4);
}

#[skuld::test]
fn touching_only_a_tracked_file_reuses_every_layer_before_copy(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"tracked");
    add_all(&repo);
    let recipe = recipe_in(
        &repo,
        "",
        &[
            "run: echo before",
            "copy: { from: git-tracked, to: \"~/p\" }",
            "run: echo after",
        ],
    );

    let first = layer_keys(&fx.engine.build(&recipe, &BuildOptions::default()).unwrap());
    // An untracked file is not in the snapshot.
    put(&repo, "untracked.txt", b"never copied");
    let untracked = layer_keys(&fx.engine.build(&recipe, &BuildOptions::default()).unwrap());
    assert_eq!(untracked, first, "an untracked file changes no layer");
    put(&repo, "a.txt", b"edited");
    let edited = layer_keys(&fx.engine.build(&recipe, &BuildOptions::default()).unwrap());

    assert_eq!(
        edited[..2],
        first[..2],
        "the base and the steps before the copy are reused"
    );
    assert_ne!(edited[2], first[2], "the copy's layer follows the files");
    assert_ne!(edited[3], first[3], "and so do the layers after it");
    assert_eq!(
        ran(&fx),
        ["echo before", "echo after", "echo after"],
        "the step before the copy ran once"
    );
}

// Pure parts ==========================================================================================================

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
fn a_tilde_user_destination_is_a_usage_error_before_any_upload(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"tracked");
    add_all(&repo);

    let error = fx
        .engine
        .build(
            &recipe_in(&repo, "", &["copy: { from: git-tracked, to: \"~root/x\" }"]),
            &BuildOptions::default(),
        )
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::Usage, "{error}");
    assert!(extractions(&fx).is_empty());
    let uploaded: Vec<_> = std::fs::read_dir(fx.server.root().join("tmp")).unwrap().collect();
    assert!(uploaded.is_empty(), "nothing was uploaded: {uploaded:?}");
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
