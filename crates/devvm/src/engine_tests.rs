use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::backend::MachineRecord;
use crate::contain::contain_tests::Sleeper;
use crate::engine::{Engine, EngineParts, ListFilter, NoHooks};
use crate::exit::Progress;
use crate::guest::BuiltinGuests;
use crate::instance::dir::InstanceDir;
use crate::instance::info::{InstanceInfo, InstanceState};
use crate::instance::record::InstanceRecord;
use crate::instance::registry::registry_tests::{TestStorage, new_instance_on};
use crate::store::layers::layers_tests::{Env, build, env, key, names, t0};
use crate::store::layers::{ChainLease, SystemClock};
use crate::test_support::fixtures::{MemoryProgress, anna_home};
use crate::test_support::host::TestHost;
use crate::{Arch, ErrorKind, GuestKind, Instance};

/// A test engine over the fake host (and, from Task 12b on, the fake backend) with its own state under the test's home.
pub(crate) struct EngineFixture {
    pub env: Env,
    pub chain: ChainLease,
    pub engine: Arc<Engine>,
}

pub(crate) fn engine_fixture(home: &Path, progress: Arc<dyn Progress>) -> EngineFixture {
    let env = env(home);
    let chain = build(&env, key(1), None);
    let engine = Engine::with_parts(EngineParts {
        dirs: env.dirs.clone(),
        host: &TestHost,
        backend: None,
        guests: Arc::new(BuiltinGuests),
        clock: Arc::new(SystemClock),
        progress,
        hooks: Arc::new(NoHooks),
    });
    EngineFixture { env, chain, engine }
}

/// An ownerless, stopped instance called `name`, created from `created_from`.
pub(crate) fn instance_in(fx: &EngineFixture, name: &str, created_from: &Path) -> InstanceDir {
    let guard = fx.env.store.begin_record().unwrap();
    let mut new = new_instance_on(&fx.chain, Some(name), &TestStorage { fail: false });
    new.created_from = created_from.to_path_buf();
    fx.engine
        .registry()
        .create(new, &guard)
        .unwrap()
        .make_ownerless(&mut || Ok(()))
        .unwrap()
}

fn memory() -> Arc<MemoryProgress> {
    Arc::new(MemoryProgress::default())
}

#[skuld::test]
fn invalid_names_are_rejected_before_any_filesystem_operation(#[fixture(anna_home)] home: &Path) {
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
fn instance_open_of_a_missing_instance_is_not_found(#[fixture(anna_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let error = Instance::open_in(&fx.engine, "nothing").err().unwrap();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert!(error.message().contains("nothing"), "{}", error.message());
}

#[skuld::test]
fn open_in_reaps_first_even_when_the_name_is_missing(#[fixture(anna_home)] home: &Path) {
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
fn list_filters_by_creation_dir_and_all_lists_everything(#[fixture(anna_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let here = home.join("projects").join("here");
    let anna = home.join("projects").join("Ж with space");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&anna).unwrap();
    instance_in(&fx, "a", &here);
    instance_in(&fx, "b", &anna);
    instance_in(&fx, "c", &here);
    instance_in(&fx, "gone", &home.join("deleted-since"));

    let listed = fx.engine.list(ListFilter::CreatedIn(here.clone())).unwrap();
    assert_eq!(info_names(&listed), ["a", "c"]);
    let listed = fx.engine.list(ListFilter::CreatedIn(anna.clone())).unwrap();
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
fn list_marks_running_by_recorded_identity(#[fixture(anna_home)] home: &Path) {
    let fx = engine_fixture(home, memory());
    let dir = instance_in(&fx, "foo", home);
    let sleeper = Sleeper::spawn();
    let record = MachineRecord {
        process: sleeper.record(),
        qmp: String::new(),
        ssh: Vec::new(),
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
fn list_ignores_a_directory_that_is_not_an_instance_name(#[fixture(anna_home)] home: &Path) {
    let memory = memory();
    let fx = engine_fixture(home, memory.clone());
    instance_in(&fx, "ok", home);
    std::fs::create_dir_all(fx.env.dirs.instances().join("Not Valid")).unwrap();
    assert_eq!(info_names(&fx.engine.list(ListFilter::All).unwrap()), ["ok"]);
    assert_eq!(memory.messages().len(), 1, "{:?}", memory.messages());
    assert!(memory.messages()[0].contains("Not Valid"));
    assert_eq!(names(&fx.env.dirs.instances()).len(), 2, "nothing is deleted");
}
