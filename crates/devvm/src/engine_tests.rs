use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::access::VmExit;
use crate::access::keys::InstanceKeys;
use crate::access::ssh_config::SshConfigEntry;
use crate::backend::fake::FakeBackend;
use crate::backend::{Backend, Endpoint, MachineRecord, SshEndpoint};
use crate::contain::contain_tests::Sleeper;
use crate::engine::{DEFAULT_BOOT_BOUND, Engine, EngineParts, LaunchHooks, ListFilter, NoHooks};
use crate::exit::Progress;
use crate::guest::Guests;
use crate::guest::fake::{FakeGuests, ServedGuests};
use crate::host::{self, Host};
use crate::instance::dir::InstanceDir;
use crate::instance::info::{InstanceInfo, InstanceState};
use crate::instance::name::InstanceName;
use crate::instance::record::{InstanceId, InstanceRecord, Marker, Owner, write_marker};
use crate::instance::registry::registry_tests::new_instance_on;
use crate::recipe::{Flattened, Resources, Source};
use crate::store::layers::layers_tests::{Env, build, env, key, names, t0};
use crate::store::layers::{ChainLease, SystemClock};
use crate::test_support::claimed::claimed;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::TestHost;
use crate::test_support::sshd::TestSshd;
use crate::test_support::stub_backend::{Attach, StubBackend};
use crate::test_support::transport::RefusingTransport;
use crate::{Arch, Dirs, ErrorKind, GuestKind, Image, Instance};
use cosca::identity::{ProcessId, ProcessIdRecord};

/// A test engine over the fake host, backend and guests, with its own state under the test's home. Its machines are
/// reached at `server`, which trusts the keys of whichever instance's seed the `guests` read last.
pub(crate) struct EngineFixture {
    pub env: Env,
    pub chain: ChainLease,
    pub engine: Arc<Engine>,
    pub fake: Arc<FakeBackend>,
    pub server: Arc<TestSshd>,
    pub guests: Arc<ServedGuests>,
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
    let server = Arc::new(TestSshd::awaiting_keys());
    let fake = Arc::new(fake.with_ssh_endpoint(server.addr()));
    let guests = Arc::new(ServedGuests::new(server.clone()));
    guests.boot_follows(fake.reset_counter());
    let engine = Engine::with_parts(EngineParts {
        dirs: env.dirs.clone(),
        host: &TestHost,
        backend: Some(wrap(fake.clone())),
        guests: guests.clone(),
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
        server,
        guests,
    }
}

/// An engine over the fixture's state with its own `host` and `backend`.
pub(crate) fn engine_over(
    fx: &EngineFixture,
    host: &'static dyn Host,
    backend: Option<Arc<dyn Backend>>,
    progress: Arc<dyn Progress>,
) -> Arc<Engine> {
    engine_with(fx.env.dirs.clone(), host, backend, Arc::new(FakeGuests), progress)
}

/// An engine over `dirs` with its own `host`, `backend` and `guests`.
pub(crate) fn engine_with(
    dirs: Dirs,
    host: &'static dyn Host,
    backend: Option<Arc<dyn Backend>>,
    guests: Arc<dyn Guests>,
    progress: Arc<dyn Progress>,
) -> Arc<Engine> {
    engine_with_bound(
        dirs,
        host,
        backend,
        guests,
        progress,
        DEFAULT_BOOT_BOUND,
        Arc::new(NoHooks),
    )
}

/// `engine_with` whose guests get `boot_bound` to answer, and whose launches `hooks` sees.
pub(crate) fn engine_with_bound(
    dirs: Dirs,
    host: &'static dyn Host,
    backend: Option<Arc<dyn Backend>>,
    guests: Arc<dyn Guests>,
    progress: Arc<dyn Progress>,
    boot_bound: Duration,
    hooks: Arc<dyn LaunchHooks>,
) -> Arc<Engine> {
    Engine::with_parts(EngineParts {
        dirs,
        host,
        backend,
        guests,
        clock: Arc::new(SystemClock),
        progress,
        hooks,
        boot_bound,
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
    let (leftovers, claim) = claimed(leftovers);
    let machine = MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
        serial_log: PathBuf::new(),
        leftovers,
        claim,
        no_reboot: false,
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
    image_with(engine, Resources::default())
}

/// `image_of` on a recipe that declares `resources`.
pub(crate) fn image_with(engine: &Arc<Engine>, resources: Resources) -> Image {
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
        resources,
        steps: Vec::new(),
    };
    Image::from_chain(engine.clone(), chain, &flat, Arch::host())
}

/// An ownerless, stopped instance called `name`, created from `created_from`.
pub(crate) fn instance_in(fx: &EngineFixture, name: &str, created_from: &Path) -> InstanceDir {
    let guard = fx.env.store.begin_record().unwrap();
    let mut new = new_instance_on(&fx.chain, Some(name), fx.fake.storage());
    new.created_from = created_from.to_path_buf();
    fx.engine
        .registry()
        .create(new, &guard)
        .unwrap()
        .make_ownerless(&mut || Ok(()))
        .unwrap()
}

/// The record of a machine whose VM process is this one, which lives as long as the test does, and whose SSH endpoint is
/// `ssh`. Its serial log is `serial.log` under `home`.
pub(crate) fn running_machine(home: &Path, ssh: std::net::SocketAddr) -> MachineRecord {
    MachineRecord {
        process: ProcessId::current()
            .to_record()
            .expect("this process has an identity record"),
        qmp: String::new(),
        ssh: vec![SshEndpoint {
            endpoint: Endpoint::Normal,
            addr: ssh,
        }],
        serial_log: home.join("serial.log"),
        leftovers: Vec::new(),
        claim: None,
        no_reboot: false,
    }
}

/// A process record that names no process: the identity of this one with another start token.
pub(crate) fn gone_process() -> ProcessIdRecord {
    let mut gone = ProcessId::current()
        .to_record()
        .expect("this process has an identity record");
    gone.token = gone.token.wrapping_add(1);
    gone
}

pub(crate) fn watched(engine: &Engine) -> Vec<ProcessIdRecord> {
    engine
        .exits
        .lock()
        .unwrap()
        .iter()
        .map(|(process, _)| process.clone())
        .collect()
}

pub(crate) fn watch_with(engine: &Engine, process: ProcessIdRecord, exit: VmExit) {
    engine.exits.lock().unwrap().push((process, exit));
}

fn ended_exit() -> VmExit {
    let exit = VmExit::when(|| Ok(())).unwrap();
    crate::access::runtime::wait(exit.gone());
    exit
}

/// A watcher that ended without knowing whether its VM lives.
fn unwatched_exit() -> VmExit {
    let exit = VmExit::when(|| Err("the VM process cannot be waited on".to_owned())).unwrap();
    crate::access::runtime::wait(exit.gone());
    exit
}

/// A watcher whose thread panicked: it ended without a verdict.
fn panicked_exit() -> VmExit {
    let exit = VmExit::when(|| panic!("the watcher thread panics")).unwrap();
    crate::access::runtime::wait(exit.gone());
    exit
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
        claim: None,
        no_reboot: false,
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

/// The five XDG directories under `home`, an unreadable `config.yaml` in them, and a dead process's leftover to reap.
/// Returns the leftover.
fn broken_config_and_a_leftover(env: &skuld::EnvGuard, home: &Path) -> PathBuf {
    for (variable, directory) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "run"),
    ] {
        env.set(variable, home.join(directory).to_str().unwrap());
    }
    let dirs = Dirs::from_env().unwrap();
    std::fs::create_dir_all(&dirs.config).unwrap();
    std::fs::write(dirs.config.join("config.yaml"), "qemu: [not, a, mapping]").unwrap();
    let leftover = dirs.instances().join(".dead-00000000000000000000000000000000");
    std::fs::create_dir_all(&leftover).unwrap();
    leftover
}

#[skuld::test]
fn from_env_reaps_before_it_fails_on_an_unreadable_config(
    #[fixture(env)] env: &skuld::EnvGuard,
    #[fixture(unicode_home)] home: &Path,
) {
    let leftover = broken_config_and_a_leftover(env, home);

    let error = match Engine::from_env(memory(), Arc::new(RefusingTransport::default())) {
        Ok(_) => panic!("an unreadable config.yaml must fail the engine"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), ErrorKind::Config);
    assert!(
        host::symlink_metadata_if_present(&leftover).unwrap().is_none(),
        "the reap ran before the config failed"
    );
}

#[skuld::test]
fn doctor_checks_gives_every_line_and_the_config_failure_on_the_qemu_line_after_the_reap(
    #[fixture(env)] env: &skuld::EnvGuard,
    #[fixture(unicode_home)] home: &Path,
) {
    let leftover = broken_config_and_a_leftover(env, home);

    let checks = Engine::doctor_checks(memory(), Arc::new(RefusingTransport::default()))
        .expect("an unreadable config.yaml does not fail doctor");

    let names: Vec<_> = checks.iter().map(|check| check.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "accelerator",
            "free space (data)",
            "free space (cache)",
            "free space (state)",
            "ssh",
            "git",
            "qemu"
        ]
    );
    let why = checks
        .last()
        .unwrap()
        .outcome
        .as_ref()
        .expect_err("the config is broken");
    assert!(why.contains("config.yaml"), "{why}");
    assert!(
        host::symlink_metadata_if_present(&leftover).unwrap().is_none(),
        "doctor reaps like every other command"
    );
}

// The exit watchers ===================================================================================================

#[skuld::test]
fn every_call_for_a_machine_shares_one_exit_watcher(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let machine = running_machine(home, fx.server.addr());

    let first = fx.engine.vm_exit(&machine).unwrap();
    let second = fx.engine.vm_exit(&machine).unwrap();

    assert_eq!(watched(&fx.engine), std::slice::from_ref(&machine.process));
    assert!(!first.has_ended() && !second.has_ended(), "this process is running");
}

#[skuld::test]
fn a_restarted_machine_gets_a_new_exit_watcher_and_the_ended_ones_entry_goes(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let restarted = running_machine(home, fx.server.addr());
    let earlier = gone_process();
    watch_with(&fx.engine, earlier, ended_exit());

    let exit = fx.engine.vm_exit(&restarted).unwrap();

    assert!(!exit.has_ended(), "the new machine's watcher is its own");
    assert_eq!(watched(&fx.engine), std::slice::from_ref(&restarted.process));
}

#[skuld::test]
fn the_watcher_of_a_live_machine_stays_while_another_one_is_asked_for(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let running = running_machine(home, fx.server.addr());
    let other = gone_process();
    // Another machine whose VM is still running: its watcher has not ended.
    watch_with(&fx.engine, other.clone(), VmExit::never());

    fx.engine.vm_exit(&running).unwrap();

    assert_eq!(watched(&fx.engine), [other, running.process]);
}

#[skuld::test]
fn an_ended_watcher_of_the_asked_machine_is_returned_as_it_is(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let machine = running_machine(home, fx.server.addr());
    watch_with(&fx.engine, machine.process.clone(), ended_exit());

    let exit = fx.engine.vm_exit(&machine).unwrap();

    assert!(exit.has_ended(), "its VM is gone, and a wait on it ends at once");
    assert_eq!(watched(&fx.engine), [machine.process]);
}

#[skuld::test]
fn an_unwatched_verdict_for_the_asked_machine_is_replaced_by_a_new_watcher(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let machine = running_machine(home, fx.server.addr());
    watch_with(&fx.engine, machine.process.clone(), unwatched_exit());

    let exit = fx.engine.vm_exit(&machine).unwrap();

    assert!(exit.verdict().is_none(), "the machine lives, and a new watcher says so");
    assert_eq!(watched(&fx.engine), [machine.process]);
}

#[skuld::test]
fn a_watcher_whose_thread_panicked_is_replaced_by_a_new_watcher(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let machine = running_machine(home, fx.server.addr());
    watch_with(&fx.engine, machine.process.clone(), panicked_exit());

    let exit = fx.engine.vm_exit(&machine).unwrap();

    assert!(exit.verdict().is_none(), "the machine lives, and a new watcher says so");
    assert_eq!(watched(&fx.engine), [machine.process]);
}

#[skuld::test]
fn a_watcher_whose_thread_panicked_for_another_machine_goes(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let running = running_machine(home, fx.server.addr());
    watch_with(&fx.engine, gone_process(), panicked_exit());

    fx.engine.vm_exit(&running).unwrap();

    assert_eq!(watched(&fx.engine), [running.process]);
}

#[skuld::test]
fn a_machine_whose_process_is_gone_has_no_exit_watcher(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let mut machine = running_machine(home, fx.server.addr());
    machine.process = gone_process();

    let error = fx.engine.vm_exit(&machine).err().expect("there is no VM to watch");

    assert_eq!(error.kind(), ErrorKind::ConnectionLost, "{error}");
    assert!(error.message().contains("the VM process exited"), "{error}");
    assert!(
        error.message().contains(&machine.serial_log.display().to_string()),
        "{error}"
    );
    assert!(watched(&fx.engine).is_empty());
}

// The ssh_config sweep ================================================================================================

/// Writes the `ssh_config` entry of an instance called `name` and returns the file it is in. It names no instance
/// directory: only the file's presence matters to the sweep.
pub(crate) fn write_conf(engine: &Engine, home: &Path, name: &str) -> PathBuf {
    let entry = SshConfigEntry {
        name: InstanceName::parse(name).unwrap(),
        id: InstanceId::random(),
        port: 2222,
        identity: home.join("keys").join("client"),
        known_hosts: home.join(format!("{name}.known_hosts")),
    };
    let configs = engine.ssh_configs();
    configs
        .write_entry(&entry, &InstanceKeys::generate().unwrap().host_public)
        .unwrap();
    configs.path().join(format!("{name}.conf"))
}

pub(crate) fn is_there(path: &Path) -> bool {
    host::symlink_metadata_if_present(path).unwrap().is_some()
}

#[skuld::test]
fn reap_sweeps_the_conf_of_an_instance_that_is_gone_and_keeps_the_others(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    instance_in(&fx, "live", home);
    let kept = write_conf(&fx.engine, home, "live");
    let swept = write_conf(&fx.engine, home, "gone");

    fx.engine.reap();

    assert!(is_there(&kept), "an instance that exists keeps its conf");
    assert!(!is_there(&swept), "no instance is called gone");
}

#[skuld::test]
fn the_sweep_keeps_the_conf_of_an_instance_whose_record_is_corrupt(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "bad", home);
    let conf = write_conf(&fx.engine, home, "bad");
    std::fs::write(dir.path().join("record.json"), "{ not json").unwrap();

    fx.engine.sweep_ssh_configs();

    assert!(is_there(&conf), "only a definite absence removes a conf");
}

#[skuld::test]
fn the_sweep_warns_and_keeps_the_conf_of_an_instance_whose_record_cannot_be_read(#[fixture(unicode_home)] home: &Path) {
    let progress = Arc::new(MemoryProgress::default());
    let fx = engine_fixture(home, progress.clone());
    let dir = instance_in(&fx, "bad", home);
    let conf = write_conf(&fx.engine, home, "bad");
    // A directory where the record should be: reading it is an I/O error, not a bad record.
    let record = dir.path().join("record.json");
    std::fs::remove_file(&record).unwrap();
    std::fs::create_dir(&record).unwrap();

    fx.engine.sweep_ssh_configs();

    assert!(
        is_there(&conf),
        "a record that cannot be read does not make its instance gone"
    );
    let messages = progress.messages();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("instance bad") && m.contains("record.json")),
        "{messages:?}"
    );
}

#[skuld::test]
fn the_sweeps_warnings_reach_the_progress_channel(#[fixture(unicode_home)] home: &Path) {
    let progress = Arc::new(MemoryProgress::default());
    let fx = engine_fixture(home, progress.clone());
    // A conf that cannot be removed: a directory under its name.
    let conf = write_conf(&fx.engine, home, "gone");
    std::fs::remove_file(&conf).unwrap();
    std::fs::create_dir(&conf).unwrap();

    fx.engine.sweep_ssh_configs();

    let messages = progress.messages();
    assert!(messages.iter().any(|m| m.contains("gone.conf")), "{messages:?}");
}

#[skuld::test]
fn reap_keeps_the_conf_of_an_instance_whose_record_it_cannot_read(#[fixture(unicode_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "newer", home);
    let conf = write_conf(&fx.engine, home, "newer");
    // Written by a devvm newer than this one: the reaper leaves the instance alone, and so does the sweep.
    std::fs::write(dir.path().join("record.json"), r#"{"schema": 4000000}"#).unwrap();

    fx.engine.reap();

    assert!(is_there(&conf));
    assert!(is_there(dir.path()), "the instance itself is left alone");
}
