//! `Engine::build` over the fake backend, with machines that launch: a fake machine is a frozen process, which only
//! Windows can spawn. The `copy` tests run `git`.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::build_tests::{
    Fixture, assert_a_failed_phase_commits_nothing, fixture, keys_of, no_instance_or_build_is_left, ran, recipe,
    recipe_in,
};
use crate::backend::fake::FakeBackend;
use crate::copy::copy_tests::{add_all, new_repo, put};
use crate::engine::ListFilter;
use crate::guest::Guests;
use crate::guest::fake::FakeGuests;
use crate::recipe::Recipe;
use crate::recipe::hash::LayerKey;
use crate::test_support::fixtures::{iso_fixture, unicode_home};
use crate::test_support::host::TestHost;
use crate::{BuildOptions, ErrorKind, GuestKind, Image};

fn layer_keys(image: &Image) -> Vec<LayerKey> {
    image.chain.refs().iter().map(|layer| layer.key).collect()
}

/// The commands of `copy` steps: the extraction of an uploaded archive.
fn extractions(fx: &Fixture) -> Vec<String> {
    fx.server
        .commands()
        .into_iter()
        .filter(|command| command.starts_with("sh -c ") && command.contains("tar -xf"))
        .collect()
}

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

/// The commands the server was given that a step ran or that flushed it, in order.
fn steps_and_flushes(fx: &Fixture) -> Vec<String> {
    fx.server
        .commands()
        .into_iter()
        .filter(|command| command.starts_with("bash -c ") || *command == flush_command() || command.contains("tar -xf"))
        .collect()
}

/// What the server is given for the guest's flush: the fake guest joins the argv with spaces.
fn flush_command() -> String {
    FakeGuests.os(GuestKind::Linux).unwrap().flush_command().argv.join(" ")
}

#[skuld::test]
fn each_step_is_followed_by_the_guests_flush_as_a_command_of_its_own(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"a");
    add_all(&repo);
    let recipe = recipe_in(
        &repo,
        "",
        &[
            "run: echo one",
            "copy: { from: git-tracked, to: \"~/p\" }",
            "run: echo two",
        ],
    );

    fx.engine.build(&recipe, &BuildOptions::default()).unwrap();

    let commands = steps_and_flushes(&fx);
    let shape: Vec<&str> = commands
        .iter()
        .map(|command| match command.as_str() {
            command if command == flush_command() => "flush",
            command if command.starts_with("bash -c ") => "run",
            _ => "extract",
        })
        .collect();
    assert_eq!(
        shape,
        ["run", "flush", "extract", "flush", "run", "flush"],
        "{commands:?}"
    );
    assert_eq!(
        commands[0], "bash -c echo one",
        "the flush is not appended to the user's script"
    );
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_flush_that_fails_commits_nothing_and_says_so(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    fx.guests.flush_runs("exit 1");
    let recipe = recipe(home, &["run: echo one"]);
    let keys = keys_of(&recipe);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    let message = error.message();
    assert!(message.contains("step 1 of 1"), "{message}");
    assert!(message.contains("flush"), "{message}");
    assert!(message.contains("exit code 1"), "{message}");
    assert!(
        fx.engine.store().unwrap().lookup(&keys[1]).unwrap().is_none(),
        "a disk whose flush failed is not a layer"
    );
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_flush_whose_session_is_lost_names_the_step_and_the_flush_and_commits_nothing(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    // The channel closes with no exit status: an error, not a status.
    fx.guests.flush_runs("close-no-status");
    let recipe = recipe(home, &["run: echo one"]);
    let keys = keys_of(&recipe);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    let message = error.message();
    assert!(message.contains("step 1 of 1"), "{message}");
    assert!(message.contains("flush"), "{message}");
    assert!(
        fx.engine.store().unwrap().lookup(&keys[1]).unwrap().is_none(),
        "a disk whose flush was lost is not a layer"
    );
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_run_step_whose_session_is_lost_names_the_step_and_its_command(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let recipe = recipe(home, &["run: echo one", "run: close-no-status"]);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    let message = error.message();
    assert!(message.contains("step 2 of 2"), "{message}");
    assert!(message.contains("`close-no-status`"), "{message}");
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_shell_the_guest_does_not_have_names_the_step_and_its_command(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let recipe = recipe(
        home,
        &["run: dir
    shell: cmd"],
    );

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    let message = error.message();
    assert!(message.contains("step 1 of 1"), "{message}");
    assert!(message.contains("`dir`"), "{message}");
    assert!(message.contains("does not exist in a Linux guest"), "{message}");
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_shutdown_that_fails_commits_nothing_and_says_so(#[fixture(unicode_home)] home: &Path) {
    let fx = assert_a_failed_phase_commits_nothing(
        home,
        FakeBackend::new(&TestHost).with_failing_shutdown(),
        "shutting the guest down",
        "does not power down",
    );
    assert_eq!(fx.fake.launches(), 1);
}

#[skuld::test]
fn a_layer_that_cannot_be_committed_names_the_step_and_publishes_nothing(#[fixture(unicode_home)] home: &Path) {
    assert_a_failed_phase_commits_nothing(
        home,
        FakeBackend::new(&TestHost).with_failing_commit(),
        "committing the step's layer",
        "the fake storage cannot commit",
    );
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
    let message = error.message();
    assert!(message.contains("step 1 of 1"), "{message}");
    assert!(
        message.contains("(resolving the destination of copy to ~root/x)"),
        "the refusal is not an upload failure: {message}"
    );
    assert!(extractions(&fx).is_empty());
    let uploaded: Vec<_> = std::fs::read_dir(fx.server.root().join("tmp")).unwrap().collect();
    assert!(uploaded.is_empty(), "nothing was uploaded: {uploaded:?}");
}

#[skuld::test]
fn an_error_of_the_layer_store_around_a_step_names_the_step(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let recipe = recipe(home, &["run: echo one", "run: echo two"]);
    let keys = keys_of(&recipe);
    // A file where the second layer's directory belongs: the store cannot read or make it.
    std::fs::create_dir_all(fx.dirs.layers()).unwrap();
    std::fs::write(fx.dirs.layers().join(keys[2].to_string()), b"in the way").unwrap();

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    let message = error.message();
    assert!(
        message.starts_with("step 2 of 2 (building the step's layer): "),
        "{message}"
    );
    assert!(
        !message.contains("step 2 of 2 (building the step's layer): step"),
        "{message}"
    );
    assert!(
        fx.engine.store().unwrap().lookup(&keys[1]).unwrap().is_some(),
        "the step before it is kept"
    );
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn a_copy_step_whose_upload_fails_names_the_step_and_the_upload_and_commits_nothing(
    #[fixture(unicode_home)] home: &Path,
) {
    let fx = fixture(home);
    let repo = new_repo(home, "repo");
    put(&repo, "a.txt", b"tracked");
    add_all(&repo);
    // The archive goes to the guest's `/tmp`; here it is a file, so nothing can be put below it.
    let tmp = fx.server.root().join("tmp");
    std::fs::remove_dir(&tmp).unwrap();
    std::fs::write(&tmp, b"not a directory").unwrap();

    let error = fx
        .engine
        .build(
            &recipe_in(&repo, "", &["copy: { from: git-tracked, to: \"~/p\" }"]),
            &BuildOptions::default(),
        )
        .err()
        .unwrap();

    let message = error.message();
    assert!(
        message.contains("step 1 of 1 (uploading the files of copy to ~/p)"),
        "{message}"
    );
    assert!(extractions(&fx).is_empty(), "the extraction ran after a failed upload");
    no_instance_or_build_is_left(&fx);
}

#[skuld::test]
fn the_iso_is_held_until_the_root_layer_is_built_and_not_while_a_step_runs(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let iso = home.join("win.iso");
    std::fs::copy(iso_fixture("single-image.iso"), &iso).unwrap();
    // Each step's guest is waited for once, before the step runs: say whether the ISO could be written then.
    let writable = Arc::new(Mutex::new(Vec::new()));
    let (seen, probed) = (writable.clone(), iso.clone());
    fx.guests.on_ready(move || {
        let open = OpenOptions::new().write(true).open(&probed);
        seen.lock().unwrap().push(open.is_ok());
    });
    let recipe = Recipe::parse(
        "from: { iso: win.iso }\nsteps:\n  - run: echo one\n",
        &home.join("r.devvm.yaml"),
    )
    .unwrap();

    fx.engine.build(&recipe, &BuildOptions::default()).unwrap();

    assert_eq!(
        *writable.lock().unwrap(),
        [true],
        "the ISO was still held while the step ran"
    );
}
