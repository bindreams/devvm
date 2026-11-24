//! `Engine::build` over the fake backend, whose machines are served by the test SSH server. The tests that launch a
//! machine need a frozen spawn, which only Windows has, and run in `build_windows_tests.rs`; these never launch one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::access::keys::InstanceKeys;
use crate::backend::Backend;
use crate::backend::fake::{FakeBackend, FakeStorage};
use crate::engine::build::{LINE_CHUNK, Lines, Tail, guest_destination, root_of};
use crate::engine::engine_tests::{engine_with, is_there, memory};
use crate::engine::{Engine, ListFilter};
use crate::guest::fake::{FakeGuests, ServedGuests};
use crate::guest::linux::{LinuxBuilder, QemuGuests};
use crate::guest::windows::builder::{Bounds, WindowsBuilder};
use crate::guest::{GuestOs, Guests};
use crate::host::symlink_metadata_if_present;
use crate::progress::{Event, Level, NullProgress};
use crate::recipe::hash::{KeyContext, LayerKey, chain_keys};
use crate::recipe::{ProductKey, Recipe};
use crate::store::fetch::{Fetcher, HttpTransport};
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
    let (root, _) = root_of(&flat.root, Arch::host(), &|_, _, _| unreachable!("no ISO here")).unwrap();
    let ctx = KeyContext {
        arch: Arch::host(),
        disk: flat.resources.disk,
        guest_semantics_version: FakeGuests.os(GuestKind::Linux).unwrap().semantics_version(),
    };
    let manifests = vec![None; flat.steps.len()];
    chain_keys(
        &flat,
        &ctx,
        &root.content(),
        &FakeGuests.builder(GuestKind::Linux).unwrap().definition_hash(),
        &manifests,
    )
    .unwrap()
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

fn iso_root(
    memo: &crate::iso::IsoMemo,
    path: &Path,
    edition: Option<&str>,
    product_key: Option<&str>,
) -> crate::Result<(super::BuildRoot, String)> {
    let source = crate::recipe::Source::Iso {
        path: path.to_owned(),
        edition: edition.map(Into::into),
        product_key: product_key.map(|text| ProductKey::parse(text).unwrap()),
    };
    root_of(&source, Arch::host(), &|iso, edition, product_key| {
        crate::iso::key_iso(&TestHost, memo, iso, edition, product_key, &MemoryProgress::default())
    })
}

#[skuld::test]
fn an_iso_root_is_keyed_by_content_and_the_image_selected(#[fixture(unicode_home)] home: &Path) {
    use crate::iso::IsoMemo;
    use crate::recipe::hash::root_key;
    let memo = IsoMemo::new(&Dirs::under(home));
    let fixture = crate::test_support::fixtures::iso_fixture;
    let (one, two, three) = (home.join("one.iso"), home.join("two.iso"), home.join("three.iso"));
    std::fs::copy(fixture("single-image.iso"), &one).unwrap();
    std::fs::copy(fixture("single-image.iso"), &two).unwrap();
    std::fs::copy(fixture("three-images.iso"), &three).unwrap();
    let root =
        |path: &Path, edition: Option<&str>, product_key: Option<&str>| iso_root(&memo, path, edition, product_key);
    let key = |path: &Path, edition: Option<&str>, product_key: Option<&str>| {
        let ctx = KeyContext {
            arch: Arch::host(),
            disk: None,
            guest_semantics_version: 1,
        };
        root_key(&ctx, &root(path, edition, product_key).unwrap().0.content(), &[0; 32])
    };
    let (only, first, second) = (
        "Windows 11 Enterprise Evaluation",
        "Windows 11 Enterprise LTSC 2024",
        "Windows 11 IoT Enterprise LTSC 2024",
    );
    let (a, b) = ("AB12C-DE34F-GH56J-KL78M-NP90Q", "AB12C-DE34F-GH56J-KL78M-NP90R");
    assert_eq!(
        key(&one, None, None),
        key(&two, None, None),
        "two paths, equal bytes, one key"
    );
    assert_ne!(key(&one, None, None), key(&three, Some(first), Some(a)));
    assert_ne!(
        key(&three, Some(first), Some(a)),
        key(&three, Some(second), Some(a)),
        "two images of one ISO"
    );
    assert_eq!(
        key(&one, None, None),
        key(&one, Some(only), None),
        "an omitted edition and the name of the only image are one install"
    );
    assert_ne!(
        key(&three, Some(first), Some(a)),
        key(&three, Some(first), Some(b)),
        "the product key is an input of the base"
    );
    // An edition devvm knows a generic key for, with no key in the recipe, is keyed as it is with that key written out.
    let generic = ProductKey::generic_for("EnterpriseS", 26100, Some(1742))
        .found()
        .unwrap();
    assert_eq!(
        key(&three, Some(first), None),
        key(&three, Some(first), Some(generic.expose())),
        "the generic key is an input of the base, as the recipe's is"
    );
    assert_ne!(key(&three, Some(first), None), key(&three, Some(first), Some(a)));
    assert_ne!(key(&three, Some(first), None), key(&three, Some(second), None));
}

#[skuld::test]
fn an_iso_root_is_refused_when_its_edition_has_no_key_or_the_key_does_not_fit_it(#[fixture(unicode_home)] home: &Path) {
    use crate::iso::IsoMemo;
    let memo = IsoMemo::new(&Dirs::under(home));
    let fixture = crate::test_support::fixtures::iso_fixture;
    let (one, three) = (home.join("one.iso"), home.join("three.iso"));
    std::fs::copy(fixture("single-image.iso"), &one).unwrap();
    std::fs::copy(fixture("three-images.iso"), &three).unwrap();
    let root =
        |path: &Path, edition: Option<&str>, product_key: Option<&str>| iso_root(&memo, path, edition, product_key);
    let (only, subscription) = (
        "Windows 11 Enterprise Evaluation",
        "Windows 11 IoT Enterprise Subscription LTSC 2024",
    );
    let a = "AB12C-DE34F-GH56J-KL78M-NP90Q";
    // Before anything boots.
    let error = root(&three, Some(subscription), None).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(
        error.message().contains(subscription) && error.message().contains("`product-key`"),
        "{error}"
    );
    root(&three, Some(subscription), Some(a)).unwrap();
    let error = root(&one, None, Some(a)).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::Recipe);
    assert!(
        error.message().contains(only) && error.message().contains("`product-key`"),
        "{error}"
    );
    assert!(!error.message().contains(a), "{error}");
}

#[skuld::test]
fn a_build_with_no_backend_fails_before_the_iso_is_read(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let engine = engine_with(dirs.clone(), &TestHost, None, Arc::new(FakeGuests), memory());
    let recipe = Recipe::parse("from: { iso: missing.iso }\nsteps: []\n", &home.join("r.devvm.yaml")).unwrap();
    let error = engine.build(&recipe, &BuildOptions::default()).err().unwrap();
    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    assert!(!dirs.iso_memo().exists());
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
fn the_linux_extract_command_is_the_one_the_engine_built_before() {
    let guest = linux();

    let command = guest
        .extract_command("/home/user/a b; rm -rf /", "/tmp/devvm-copy-x.tar")
        .unwrap();

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
fn a_linux_recipe_with_set_uac_is_refused_naming_the_guest_not_silently_accepted(#[fixture(unicode_home)] home: &Path) {
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

/// A refused `set` step is found before anything is built or keyed: no base is built, no guest is asked, and the
/// store, the build directories and the ISO memo are not made.
#[skuld::test]
fn a_refused_set_step_fails_before_any_base_is_built_or_keyed(#[fixture(unicode_home)] home: &Path) {
    let fx = fixture(home);
    let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = built.clone();
    fx.guests
        .on_base(move || _ = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
    let recipe = recipe(home, &["run: echo one", "run: echo two", "set: { uac: auto-approve }"]);

    let error = fx.engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    assert_eq!(error.kind(), ErrorKind::Recipe, "{error}");
    assert!(error.message().contains("step 3 of 3"), "{error}");
    assert_eq!(built.load(std::sync::atomic::Ordering::SeqCst), 0, "a base was built");
    assert!(fx.server.commands().is_empty(), "a guest ran a command");
    assert!(!is_there(&fx.dirs.builds()), "a build directory was made");
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
    let tail = Tail::default();
    let mut lines = Lines::new(&progress, &tail);
    for chunk in chunks {
        lines.write_all(chunk).unwrap();
    }
    drop(lines);
    progress.messages()
}

#[skuld::test]
fn step_output_arrives_as_output_events_not_notes() {
    let progress = MemoryProgress::default();
    let tail = Tail::default();
    let mut lines = Lines::new(&progress, &tail);
    lines.write_all(b"one\ntwo\n").unwrap();
    drop(lines);
    assert_eq!(
        progress.events(),
        [
            Event::Output { line: "one".to_owned() },
            Event::Output { line: "two".to_owned() },
        ]
    );
    assert!(!progress.events().iter().any(|event| matches!(
        event,
        Event::Note {
            level: Level::Info | Level::Warn,
            ..
        }
    )));
}

#[skuld::test]
fn the_tail_keeps_the_last_20_lines_of_both_streams_in_order() {
    let progress = MemoryProgress::default();
    let tail = Tail::default();
    let (mut out, mut err) = (Lines::new(&progress, &tail), Lines::new(&progress, &tail));
    for n in 1..=25 {
        let stream = if n % 2 == 0 { &mut out } else { &mut err };
        writeln!(stream, "line {n}").unwrap();
    }
    drop((out, err));
    let expected: Vec<String> = (6..=25).map(|n| format!("line {n}")).collect();
    assert_eq!(tail.text(), expected.join("\n"));
}

#[skuld::test]
fn a_step_that_printed_nothing_has_an_empty_tail() {
    assert_eq!(Tail::default().text(), "");
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

#[skuld::test]
fn a_blank_line_ends_in_a_carriage_return_and_line_feed_like_in_a_line_feed() {
    assert_eq!(lines_reported(&[b"a\r\n\r\nb\r\n"]), ["a", "", "b"]);
    assert_eq!(lines_reported(&[b"\r\nb\r\n"]), ["", "b"]);
    assert_eq!(
        lines_reported(&[b"a\r\n\r", b"\nb\r\n"]),
        ["a", "", "b"],
        "wherever the writes split them"
    );
    assert_eq!(lines_reported(&[b"a\r\n", b"\r", b"\n", b"b\n"]), ["a", "", "b"]);
}

#[skuld::test]
fn a_carriage_return_after_a_line_end_is_a_redraw_when_text_follows_it() {
    assert_eq!(lines_reported(&[b"a\r\n\r", b"10%\r100%\r\n"]), ["a", "10%", "100%"]);
    assert_eq!(
        lines_reported(&[b"a\r\n\r"]),
        ["a"],
        "a lone carriage return at the end reports nothing"
    );
}

/// `len` bytes of text with no line end.
fn long_line(len: usize) -> String {
    (0..len).map(|n| char::from(b'a' + (n % 26) as u8)).collect()
}

#[skuld::test]
fn a_line_without_a_line_end_is_reported_in_chunks_as_it_grows() {
    let progress = MemoryProgress::default();
    let tail = Tail::default();
    let mut lines = Lines::new(&progress, &tail);
    let text = long_line(2 * LINE_CHUNK + 5);

    lines.write_all(text.as_bytes()).unwrap();
    assert_eq!(progress.messages().len(), 2, "two full chunks are out before the end");
    drop(lines);

    let reported = progress.messages();
    assert_eq!(reported.len(), 3);
    assert_eq!(reported.concat(), text);
    assert!(reported[..2].iter().all(|line| line.len() == LINE_CHUNK));
}

#[skuld::test]
fn a_line_of_exactly_a_chunk_is_one_line() {
    let text = long_line(LINE_CHUNK);
    for end in [&b"\n"[..], b"\r", b"\r\n"] {
        assert_eq!(
            lines_reported(&[text.as_bytes(), end, b"next\n"]),
            [text.clone(), "next".to_owned()],
            "{end:?}"
        );
    }
}

#[skuld::test]
fn a_flushed_part_of_a_line_leaves_the_line_end_and_the_redraw_after_it_as_they_were() {
    let text = long_line(LINE_CHUNK + 3);
    let (first, rest) = text.split_at(LINE_CHUNK);
    for end in [&b"\n"[..], b"\r\n", b"\r"] {
        let mut input = text.clone().into_bytes();
        input.extend_from_slice(end);
        input.extend_from_slice(b"next\n");
        assert_eq!(lines_reported(&[&input]), [first, rest, "next"], "{end:?}");
    }
    // The line end arrives in writes of its own after the flush.
    assert_eq!(
        lines_reported(&[text.as_bytes(), b"\r", b"\n", b"next\n"]),
        [first, rest, "next"]
    );
}

#[skuld::test]
fn a_chunk_never_splits_a_character() {
    // `é` is two bytes: the first falls on the last byte of the chunk.
    let mut text = "a".repeat(LINE_CHUNK - 1).into_bytes();
    text.extend_from_slice("é".as_bytes());
    text.extend_from_slice(b"tail\n");

    let reported = lines_reported(&[&text]);

    assert_eq!(reported.concat(), format!("{}étail", "a".repeat(LINE_CHUNK - 1)));
    assert!(!reported.concat().contains('\u{fffd}'), "{reported:?}");

    // An invalid byte earlier in the chunk does not hide the cut character.
    text[0] = 0xff;
    let reported = lines_reported(&[&text]);
    assert_eq!(
        reported.concat().matches('\u{fffd}').count(),
        1,
        "only the invalid byte is replaced: {reported:?}"
    );
}

#[skuld::test]
fn invalid_utf8_after_an_escape_is_reported_in_chunks_without_a_line_end() {
    // Each invalid byte reports as a 3-byte U+FFFD, so a part rebuilt from its lossy text would outgrow the chunk.
    let parts = 16;
    let mut input = b"a\x1b[".to_vec();
    input.resize(parts * LINE_CHUNK + 1, 0xff);

    let progress = MemoryProgress::default();
    let tail = Tail::default();
    let mut lines = Lines::new(&progress, &tail);
    lines.write_all(&input).unwrap();
    assert_eq!(progress.messages().len(), parts, "a part is out for each full chunk");
    drop(lines);

    let reported = progress.messages();
    assert_eq!(reported.len(), parts + 1);
    assert!(reported[1..parts].iter().all(|part| part.len() == 3 * LINE_CHUNK));
}

#[skuld::test]
fn a_long_line_below_the_bound_is_one_entry_in_the_tail() {
    // A linker command line, as rustc prints it after an error.
    let command = format!("  = note: \"cc\" {}", long_line(30_000));
    let progress = MemoryProgress::default();
    let tail = Tail::default();
    let mut lines = Lines::new(&progress, &tail);
    lines
        .write_all(format!("error: linking failed\n{command}\n").as_bytes())
        .unwrap();
    drop(lines);

    assert_eq!(tail.text(), format!("error: linking failed\n{command}"));
    assert_eq!(progress.messages(), ["error: linking failed", command.as_str()]);
}

#[skuld::test]
fn a_stream_of_unterminated_output_keeps_a_bounded_tail() {
    let progress = MemoryProgress::default();
    let tail = Tail::default();
    let mut lines = Lines::new(&progress, &tail);
    lines.write_all(long_line(50 * LINE_CHUNK).as_bytes()).unwrap();
    drop(lines);
    assert_eq!(tail.text().lines().count(), crate::error::TAIL_LINES);
}

/// The guests of the QEMU backend: a Windows recipe's guest is the real one, whose shells are the real ones.
fn qemu_guests(home: &Path) -> Arc<dyn Guests> {
    let fetcher = Arc::new(Fetcher::new(
        Dirs::under(home).downloads(),
        Arc::new(HttpTransport::new().unwrap()),
        Arc::new(NullProgress),
    ));
    Arc::new(QemuGuests::new(
        Arc::new(LinuxBuilder::new(fetcher.clone(), Arc::new(FakeStorage::default()))),
        Arc::new(WindowsBuilder::new(
            fetcher,
            Arc::new(FakeBackend::new(&TestHost)),
            &TestHost,
            Bounds::default(),
        )),
    ))
}

/// A `run` step whose command the guest cannot be given is found before anything is built or keyed, like a refused
/// `set` step: it is the recipe's mistake, and a build of an hour would otherwise end at its step. The engine has no
/// backend and the ISO is missing: either would be the error if the step were not looked at first.
#[skuld::test]
fn a_run_step_the_guest_cannot_be_given_fails_before_any_base_is_built(#[fixture(unicode_home)] home: &Path) {
    let oversize = format!("run: {}", "x".repeat(40_000));
    for (step, kind, say) in [
        (
            "run: |\n      echo a\n      echo b\n    shell: cmd",
            ErrorKind::Recipe,
            "pwsh",
        ),
        ("run: echo hi\n    shell: sh", ErrorKind::Recipe, "windows"),
        // Refused by the guest's wrapping, which the step's run would reach only after the steps before it were built.
        (oversize.as_str(), ErrorKind::Usage, "too long for a Windows guest"),
    ] {
        let dirs = Dirs::under(home);
        let engine = engine_with(dirs.clone(), &TestHost, None, qemu_guests(home), memory());
        let recipe = Recipe::parse(
            &format!("from: {{ iso: missing.iso }}\nsteps:\n  - {step}\n"),
            &home.join("r.devvm.yaml"),
        )
        .unwrap();

        let error = engine.build(&recipe, &BuildOptions::default()).err().unwrap();

        assert_eq!(error.kind(), kind, "{error}");
        assert!(error.message().contains("step 1 of 1"), "{error}");
        assert!(error.message().contains(say), "{error}");
        assert!(!dirs.iso_memo().exists() && !is_there(&dirs.builds()), "{error}");
    }
}

/// The extraction of a `copy` step is a PowerShell script on a Windows guest, so the step passes the one check of the
/// recipe's commands: with no backend, the build ends where it needs one, after the check, instead of at the step.
#[skuld::test]
fn a_copy_step_passes_the_upfront_check_on_a_windows_guest(#[fixture(unicode_home)] home: &Path) {
    let dirs = Dirs::under(home);
    let engine = engine_with(dirs.clone(), &TestHost, None, qemu_guests(home), memory());
    let recipe = Recipe::parse(
        "from: { iso: missing.iso }\nsteps:\n  - copy: { from: git-tracked, to: C:/x }\n",
        &home.join("r.devvm.yaml"),
    )
    .unwrap();

    let error = engine.build(&recipe, &BuildOptions::default()).err().unwrap();

    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    assert!(error.message().contains("no VM backend"), "{error}");
    assert!(!error.message().contains("step 1 of 1"), "{error}");
}

// A Windows base's expiry =============================================================================================

/// The build of an ISO root whose base builder says the base ends at `expires`: the guests are the fake ones, so no
/// Windows is installed. Nothing starts, as the recipe has no step.
fn build_with_a_base_that_ends(home: &Path, expires: std::time::SystemTime) -> crate::Error {
    let iso = home.join("windows.iso");
    std::fs::copy(crate::test_support::fixtures::iso_fixture("single-image.iso"), &iso).unwrap();
    let engine = engine_with(
        Dirs::under(home),
        &TestHost,
        Some(Arc::new(FakeBackend::new(&TestHost))),
        Arc::new(crate::guest::fake::ExpiringGuests(expires)),
        memory(),
    );
    let recipe = Recipe::parse(
        &format!(
            "from: {{ iso: {} }}\nsteps: []\n",
            serde_json::to_string(iso.to_str().unwrap()).unwrap()
        ),
        &home.join("r.devvm.yaml"),
    )
    .unwrap();

    engine
        .build(&recipe, &BuildOptions::default())
        .err()
        .expect("the base is too close to its end")
}

#[skuld::test]
fn a_base_born_within_ten_days_is_expired_base_naming_date_and_iso(#[fixture(unicode_home)] home: &Path) {
    let expires = std::time::SystemTime::now() + std::time::Duration::from_secs(5 * 24 * 3600);

    let error = build_with_a_base_that_ends(home, expires);

    assert_eq!(error.kind(), ErrorKind::ExpiredBase, "{error}");
    assert!(error.message().contains("within 10 days"), "{error}");
    assert!(error.message().contains("windows.iso"), "{error}");
    assert!(
        error.message().contains(&crate::store::layers::date_text(expires)),
        "{error}"
    );
}

#[skuld::test]
fn a_base_that_ends_now_is_expired_base(#[fixture(unicode_home)] home: &Path) {
    let error = build_with_a_base_that_ends(home, std::time::SystemTime::now());

    assert_eq!(error.kind(), ErrorKind::ExpiredBase, "{error}");
}

#[skuld::test]
fn step_output_has_no_colour_escapes_for_the_progress_or_the_tail() {
    let progress = MemoryProgress::default();
    let tail = Tail::default();
    let mut lines = Lines::new(&progress, &tail);
    lines.write_all(b"\x1b[31merror\x1b[0m: boom\n").unwrap();
    drop(lines);
    assert_eq!(tail.text(), "error: boom");
    assert_eq!(progress.messages(), ["error: boom"]);
}
