use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::backend::fake::{FakeBackend, FakeStorage};
use crate::backend::{Backend, MachineRecord};
use crate::contain::contain_tests::Sleeper;
use crate::engine::{DEFAULT_BOOT_BOUND, Engine, EngineParts, LaunchHooks, ListFilter, NoHooks};
use crate::exit::Progress;
use crate::guest::fake::FakeGuests;
use crate::host::Host;
use crate::instance::dir::InstanceDir;
use crate::instance::info::{InstanceInfo, InstanceState};
use crate::instance::record::{InstanceId, InstanceRecord, Marker, Owner, write_marker};
use crate::instance::registry::registry_tests::new_instance_on;
use crate::recipe::{Flattened, Resources, Source};
use crate::store::layers::layers_tests::{Env, build, env, key, names, t0};
use crate::store::layers::{ChainLease, SystemClock};
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::stub_backend::{Attach, StubBackend};
use crate::{Arch, ErrorKind, GuestKind, Image, Instance};

/// A test engine over the fake host, backend and guests, with its own state under the test's home.
pub(crate) struct EngineFixture {
    pub env: Env,
    pub chain: ChainLease,
    pub engine: Arc<Engine>,
    pub fake: Arc<FakeBackend>,
}

pub(crate) fn engine_fixture(home: &Path, progress: Arc<dyn Progress>) -> EngineFixture {
    engine_fixture_with(home, progress, Arc::new(NoHooks), |fake| fake)
}

/// `engine_fixture` with launch `hooks`, and with the engine reaching the fake backend through `wrap`.
pub(crate) fn engine_fixture_with(
    home: &Path,
    progress: Arc<dyn Progress>,
    hooks: Arc<dyn LaunchHooks>,
    wrap: impl FnOnce(Arc<FakeBackend>) -> Arc<dyn Backend>,
) -> EngineFixture {
    engine_fixture_from(home, progress, hooks, FakeBackend::new(&TestHost), wrap)
}

/// `engine_fixture` over a backend whose machines create files outside their instance directory (under `home/run`) and
/// do not delete them when killed or shut down: everything that ends a machine must.
pub(crate) fn leaking_fixture(home: &Path, progress: Arc<dyn Progress>) -> EngineFixture {
    engine_fixture_from(home, progress, Arc::new(NoHooks), leaking_backend(home), |fake| fake)
}

/// The fake backend of [`leaking_fixture`].
pub(crate) fn leaking_backend(home: &Path) -> FakeBackend {
    let run_dir = home.join("run");
    std::fs::create_dir_all(&run_dir).unwrap();
    FakeBackend::new(&TestHost).with_leftovers_in(&run_dir, false)
}

pub(crate) fn engine_fixture_from(
    home: &Path,
    progress: Arc<dyn Progress>,
    hooks: Arc<dyn LaunchHooks>,
    fake: FakeBackend,
    wrap: impl FnOnce(Arc<FakeBackend>) -> Arc<dyn Backend>,
) -> EngineFixture {
    let env = env(home);
    let chain = build(&env, key(1), None);
    let fake = Arc::new(fake);
    let engine = Engine::with_parts(EngineParts {
        dirs: env.dirs.clone(),
        host: &TestHost,
        backend: Some(wrap(fake.clone())),
        guests: Arc::new(FakeGuests),
        clock: Arc::new(SystemClock),
        progress,
        hooks,
        boot_bound: DEFAULT_BOOT_BOUND,
    });
    EngineFixture {
        env,
        chain,
        engine,
        fake,
    }
}

/// An engine over the fixture's state with its own `host` and `backend`.
pub(crate) fn engine_over(
    fx: &EngineFixture,
    host: &'static dyn Host,
    backend: Option<Arc<dyn Backend>>,
    progress: Arc<dyn Progress>,
) -> Arc<Engine> {
    Engine::with_parts(EngineParts {
        dirs: fx.env.dirs.clone(),
        host,
        backend,
        guests: Arc::new(FakeGuests),
        clock: Arc::new(SystemClock),
        progress,
        hooks: Arc::new(NoHooks),
        boot_bound: DEFAULT_BOOT_BOUND,
    })
}

/// An engine whose backend is the scripted [`StubBackend`], with the progress it reports to.
pub(crate) fn stub_fixture(home: &Path, attach: Attach) -> (EngineFixture, Arc<StubBackend>, Arc<MemoryProgress>) {
    let stub = StubBackend::new(attach);
    let progress = memory();
    let found = stub.clone();
    let fx = engine_fixture_with(home, progress.clone(), Arc::new(NoHooks), move |_| found);
    (fx, stub, progress)
}

/// The record of a machine whose process is gone, naming `leftovers`.
pub(crate) fn dead_machine(leftovers: Vec<PathBuf>) -> MachineRecord {
    let sleeper = Sleeper::spawn();
    let machine = MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
        serial_log: PathBuf::new(),
        leftovers,
    };
    sleeper.finish();
    machine
}

/// Records `machine` as the machine of `dir`.
pub(crate) fn record_machine(dir: &InstanceDir, machine: &MachineRecord) {
    let op = dir.op_exclusive().unwrap();
    dir.update(&op, &mut |r: &mut InstanceRecord| r.machine = Some(machine.clone()))
        .unwrap();
}

/// The image on the fixture's layer (key 1), leased through the engine's own store.
pub(crate) fn image_of(engine: &Arc<Engine>) -> Image {
    let chain = engine
        .store()
        .unwrap()
        .lookup(&key(1))
        .unwrap()
        .expect("the fixture built layer 1");
    let flat = Flattened {
        name: "test".to_owned(),
        guest: GuestKind::Linux,
        root: Source::Embedded("test".to_owned()),
        resources: Resources::default(),
        steps: Vec::new(),
    };
    Image::from_chain(engine.clone(), chain, &flat)
}

/// An ownerless, stopped instance called `name`, created from `created_from`.
pub(crate) fn instance_in(fx: &EngineFixture, name: &str, created_from: &Path) -> InstanceDir {
    let guard = fx.env.store.begin_record().unwrap();
    let mut new = new_instance_on(&fx.chain, Some(name), &FakeStorage);
    new.created_from = created_from.to_path_buf();
    fx.engine
        .registry()
        .create(new, &guard)
        .unwrap()
        .make_ownerless(&mut || Ok(()))
        .unwrap()
}

pub(crate) fn memory() -> Arc<MemoryProgress> {
    Arc::new(MemoryProgress::default())
}

#[skuld::test]
fn invalid_names_are_rejected_before_any_filesystem_operation(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    for text in ["../x", "con", "a*", ""] {
        let error = Instance::open_in(&fx.engine, text).err().unwrap();
        assert_eq!(error.kind(), ErrorKind::InvalidName, "{text:?}");
    }
    assert!(
        !fx.env.dirs.state.exists(),
        "no state directory is created for a bad name"
    );
}

#[skuld::test]
fn instance_open_of_a_missing_instance_is_not_found(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let error = Instance::open_in(&fx.engine, "nothing").err().unwrap();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.message().contains("nothing"), "{}", error.message());
}

#[skuld::test]
fn open_in_reaps_first_even_when_the_name_is_missing(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    // A creator that died: its `.new-` directory is left, and nobody holds its owner lock.
    let id = crate::instance::record::InstanceId::random();
    drop(fx.engine.registry().locks().acquire_owner_new(&id).unwrap());
    let leftover = fx.env.dirs.instances().join(format!(".new-{id}"));
    std::fs::create_dir_all(&leftover).unwrap();

    assert_eq!(
        Instance::open_in(&fx.engine, "nothing").err().unwrap().kind(),
        ErrorKind::NotFound
    );
    assert!(!leftover.exists(), "opening an instance reaps leftovers first");
}

fn info_names(infos: &[InstanceInfo]) -> Vec<&str> {
    let mut names: Vec<&str> = infos.iter().map(|info| info.name.as_str()).collect();
    names.sort_unstable();
    names
}

#[skuld::test]
fn list_filters_by_creation_dir_and_all_lists_everything(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let here = home.join("projects").join("here");
    let project_dir = home.join("projects").join("Ж with space");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&project_dir).unwrap();
    instance_in(&fx, "a", &here);
    instance_in(&fx, "b", &project_dir);
    instance_in(&fx, "c", &here);
    instance_in(&fx, "gone", &home.join("deleted-since"));

    let listed = fx.engine.list(ListFilter::CreatedIn(here.clone())).unwrap();
    assert_eq!(info_names(&listed), ["a", "c"]);
    let listed = fx.engine.list(ListFilter::CreatedIn(project_dir.clone())).unwrap();
    assert_eq!(info_names(&listed), ["b"]);
    let listed = fx
        .engine
        .list(ListFilter::CreatedIn(home.join("deleted-since")))
        .unwrap();
    assert_eq!(
        info_names(&listed),
        ["gone"],
        "a directory that no longer exists matches by its raw path"
    );
    assert_eq!(fx.engine.list(ListFilter::All).unwrap().len(), 4);
    assert!(listed[0].ownerless);
    assert_eq!(listed[0].guest, GuestKind::Linux);
    assert_eq!(listed[0].arch, Arch::Amd64);
}

#[skuld::test]
fn list_matches_a_creation_dir_given_in_another_spelling_of_the_same_directory(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let here = home.join("projects").join("here");
    let elsewhere = home.join("projects").join("elsewhere");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    instance_in(&fx, "a", &here);
    instance_in(&fx, "b", &elsewhere);

    // The same directory through a `..` segment: not the path the record holds, but the directory it names.
    let spelled = elsewhere.join("..").join("here");
    assert_ne!(spelled, here);
    let listed = fx.engine.list(ListFilter::CreatedIn(spelled)).unwrap();

    assert_eq!(info_names(&listed), ["a"]);
}

#[skuld::test]
fn list_marks_running_by_recorded_identity(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    let sleeper = Sleeper::spawn();
    let record = MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
        serial_log: std::path::PathBuf::new(),
        leftovers: Vec::new(),
    };
    let op = dir.op_exclusive().unwrap();
    dir.update(&op, &mut |r: &mut InstanceRecord| r.machine = Some(record.clone()))
        .unwrap();
    drop(op);
    assert_eq!(
        fx.engine.list(ListFilter::All).unwrap()[0].state,
        InstanceState::Running
    );

    let mut forged = record.clone();
    forged.process.token ^= 1;
    let op = dir.op_exclusive().unwrap();
    dir.update(&op, &mut |r: &mut InstanceRecord| r.machine = Some(forged.clone()))
        .unwrap();
    drop(op);
    assert_eq!(
        fx.engine.list(ListFilter::All).unwrap()[0].state,
        InstanceState::Stopped,
        "another process with the recorded PID is not the VM"
    );
}

#[skuld::test]
fn to_json_line_is_one_line_with_the_documented_keys() {
    let info = InstanceInfo {
        name: "foo".to_owned(),
        state: InstanceState::Stopped,
        guest: GuestKind::Linux,
        arch: Arch::Amd64,
        created: t0() - Duration::from_secs(100_000_000),
        created_from: PathBuf::from("/work/Ж project"),
        expires: None,
        ownerless: true,
    };
    let line = info.to_json_line();
    assert!(!line.contains('\n'));
    assert_eq!(
        line,
        r#"{"name":"foo","state":"stopped","guest":"linux","arch":"amd64","created":"2023-11-14T22:13:20Z","created_from":"/work/Ж project","expires":null,"owner":"none"}"#
    );
    let owned = InstanceInfo {
        state: InstanceState::Running,
        expires: Some(t0()),
        ownerless: false,
        ..info
    };
    let line = owned.to_json_line();
    assert!(
        line.contains(r#""state":"running""#) && line.contains(r#""owner":"process""#),
        "{line}"
    );
    assert!(line.contains(r#""expires":"2027-01-15T08:00:00Z""#), "{line}");
}

#[skuld::test]
fn list_ignores_a_directory_that_is_not_an_instance_name(#[fixture(unicode_home)] home: &Path) {
    let memory = memory();
    let fx = engine_fixture(home, memory.clone());
    instance_in(&fx, "ok", home);
    std::fs::create_dir_all(fx.env.dirs.instances().join("Not Valid")).unwrap();
    assert_eq!(info_names(&fx.engine.list(ListFilter::All).unwrap()), ["ok"]);
    assert_eq!(memory.messages().len(), 1, "{:?}", memory.messages());
    assert!(memory.messages()[0].contains("Not Valid"));
    assert_eq!(names(&fx.env.dirs.instances()).len(), 2, "nothing is deleted");
}

#[skuld::test]
fn list_says_nothing_about_an_instance_removed_while_it_lists(#[fixture(unicode_home)] home: &Path) {
    let progress = memory();
    let fx = engine_fixture(home, progress.clone());
    instance_in(&fx, "foo", home);

    let infos = fx
        .engine
        .list_with(ListFilter::All, &mut |dir| {
            let op = dir.op_exclusive().unwrap();
            dir.destroy(op).unwrap();
        })
        .unwrap();

    assert!(infos.is_empty());
    assert!(progress.messages().is_empty(), "{:?}", progress.messages());
}

#[skuld::test]
fn list_still_warns_about_an_instance_with_an_unreadable_marker(#[fixture(unicode_home)] home: &Path) {
    let progress = memory();
    let fx = engine_fixture(home, progress.clone());
    let dir = instance_in(&fx, "foo", home);
    std::fs::remove_file(dir.path().join("owner.json")).unwrap();

    assert!(fx.engine.list(ListFilter::All).unwrap().is_empty());
    let messages = progress.messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("foo") && messages[0].contains("owner.json"),
        "{messages:?}"
    );
}

#[skuld::test]
fn list_names_why_a_marker_is_unusable(#[fixture(unicode_home)] home: &Path) {
    let progress = memory();
    let fx = engine_fixture(home, progress.clone());
    let torn = instance_in(&fx, "torn", home);
    std::fs::write(torn.path().join("owner.json"), "{ torn").unwrap();
    let newer = instance_in(&fx, "newer", home);
    std::fs::write(newer.path().join("owner.json"), r#"{"schema": 2}"#).unwrap();

    assert!(fx.engine.list(ListFilter::All).unwrap().is_empty());

    let messages = progress.messages();
    assert_eq!(messages.len(), 2, "{messages:?}");
    let about = |name: &str| {
        messages
            .iter()
            .find(|m| m.contains(name))
            .unwrap_or_else(|| panic!("{messages:?}"))
    };
    assert!(
        about("torn").contains("line 1 column"),
        "the parse error is lost: {}",
        about("torn")
    );
    assert!(
        about("newer").contains("newer devvm"),
        "the read error is lost: {}",
        about("newer")
    );
}

#[skuld::test]
fn list_says_nothing_about_an_instance_whose_name_was_reused_while_it_lists(#[fixture(unicode_home)] home: &Path) {
    let progress = memory();
    let fx = engine_fixture(home, progress.clone());
    instance_in(&fx, "foo", home);

    let infos = fx
        .engine
        .list_with(ListFilter::All, &mut |dir| {
            // The marker of another instance that now has the name.
            write_marker(
                dir.path(),
                &Marker {
                    owner: Owner::Process,
                    id: InstanceId::random(),
                },
            )
            .unwrap();
        })
        .unwrap();

    assert!(infos.is_empty(), "{infos:?}");
    assert!(progress.messages().is_empty(), "{:?}", progress.messages());
}

#[skuld::test]
fn from_env_waits_ten_minutes_for_a_boot() {
    let engine = Engine::from_env(Arc::new(MemoryProgress::default())).unwrap();
    assert_eq!(engine.boot_bound(), Duration::from_secs(10 * 60));
}

#[skuld::test]
fn with_parts_keeps_the_boot_bound_it_is_given(#[fixture(unicode_home)] home: &Path) {
    let env = env(home);
    let engine = Engine::with_parts(EngineParts {
        dirs: env.dirs.clone(),
        host: &TestHost,
        backend: None,
        guests: Arc::new(FakeGuests),
        clock: Arc::new(SystemClock),
        progress: Arc::new(MemoryProgress::default()),
        hooks: Arc::new(NoHooks),
        boot_bound: Duration::from_millis(250),
    });
    assert_eq!(engine.boot_bound(), Duration::from_millis(250));
}

#[skuld::test]
fn list_asks_the_backend_whether_a_recorded_machine_runs(#[fixture(unicode_home)] home: &Path) {
    let (fx, stub, _) = stub_fixture(home, Attach::Running);
    let dir = instance_in(&fx, "foo", home);
    record_machine(&dir, &dead_machine(Vec::new()));

    let state = |fx: &EngineFixture| fx.engine.list(ListFilter::All).unwrap()[0].state;
    assert_eq!(
        state(&fx),
        InstanceState::Running,
        "the backend decides, not the process table"
    );
    stub.answer(Attach::Gone);
    assert_eq!(state(&fx), InstanceState::Stopped);
}

#[skuld::test]
fn list_cannot_tell_when_the_backend_cannot_look(#[fixture(unicode_home)] home: &Path) {
    let (fx, _, progress) = stub_fixture(home, Attach::Fails(ErrorKind::Io));
    let dir = instance_in(&fx, "foo", home);
    record_machine(&dir, &dead_machine(Vec::new()));

    assert_eq!(
        fx.engine.list(ListFilter::All).unwrap()[0].state,
        InstanceState::Unknown
    );
    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("cannot tell whether instance foo is running") && m.contains("cannot look")),
        "{messages:?}"
    );
}

#[skuld::test]
fn list_cannot_tell_for_a_recorded_machine_when_the_build_has_no_backend(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    record_machine(&dir, &dead_machine(Vec::new()));
    let progress = memory();
    let engine = engine_over(&fx, &TestHost, None, progress.clone());

    let infos = engine.list(ListFilter::All).unwrap();

    assert_eq!(infos[0].state, InstanceState::Unknown);
    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("foo") && m.contains("no VM backend")),
        "the cause is the missing backend: {messages:?}"
    );
}
