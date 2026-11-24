use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cosca::identity::{Platform, ProcessIdRecord};

use crate::backend::{Endpoint, MachineRecord, SshEndpoint};
use crate::instance::name::InstanceName;
use crate::instance::record::{
    HOLDER_FILE, HOSTNAME_LEN, Holder, InstanceId, InstanceRecord, Marker, MarkerRead, Owner, RecordRead, check_schema,
    program_name, read_holder, read_marker, remove_holder, write_holder, write_marker,
};
use crate::recipe::hash::LayerKey;
use crate::store::layers::{Generation, LayerRef};
use crate::test_support::fixtures::{non_unicode_name, unicode_home};
use crate::{Arch, ErrorKind, GuestKind, Network};

fn id() -> InstanceId {
    "000102030405060708090a0b0c0d0e0f".parse().unwrap()
}

#[skuld::test]
fn the_hostname_is_devvm_and_the_first_eight_hex_digits_of_the_id() {
    assert_eq!(id().hostname(), "devvm-00010203");
    let other: InstanceId = "ffeeddccbbaa99887766554433221100".parse().unwrap();
    assert_eq!(other.hostname(), "devvm-ffeeddcc");
    assert_eq!(other.hostname().len(), HOSTNAME_LEN);
}

fn layer(n: u8) -> LayerRef {
    LayerRef {
        key: LayerKey::from_bytes([n; 16]),
        generation: Generation::from_bytes([n + 1; 16]),
    }
}

fn machine() -> MachineRecord {
    MachineRecord {
        process: ProcessIdRecord {
            version: 2,
            platform: Platform::Windows,
            pid: 4242,
            token: 133_000_000_000_000_000,
            boot_id: None,
            pid_ns: None,
            token_kind: None,
        },
        qmp: "/run/devvm-ab.sock".to_owned(),
        serial_log: std::path::PathBuf::from("/state/serial.log"),
        leftovers: Vec::new(),
        claim: None,
        ssh: vec![SshEndpoint {
            endpoint: Endpoint::Normal,
            addr: "127.0.0.1:2222".parse().unwrap(),
        }],
        no_reboot: false,
    }
}

fn record(created_from: PathBuf) -> InstanceRecord {
    InstanceRecord {
        id: id(),
        name: InstanceName::parse("foo").unwrap(),
        created: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        created_from,
        guest: GuestKind::Linux,
        arch: Arch::Amd64,
        chain: vec![layer(1), layer(3)],
        expires: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)),
        cpus: 2,
        memory: 4 << 30,
        network: Network::Nat,
        machine: Some(machine()),
    }
}

/// The record a devvm with schema 1 writes. `devvm-cli`'s `ls` tests seed their instances from this text, so a change
/// here is a format change.
const GOLDEN: &str = r#"{
  "schema": 1,
  "id": "000102030405060708090a0b0c0d0e0f",
  "name": "foo",
  "created": "2023-11-14T22:13:20Z",
  "created_from": "/work/project",
  "guest": "linux",
  "arch": "amd64",
  "chain": [
    {
      "key": "01010101010101010101010101010101",
      "generation": "02020202020202020202020202020202"
    },
    {
      "key": "03030303030303030303030303030303",
      "generation": "04040404040404040404040404040404"
    }
  ],
  "expires": "2027-01-15T08:00:00Z",
  "cpus": 2,
  "memory": 4294967296,
  "network": "nat",
  "machine": {
    "process": {
      "v": 2,
      "platform": "windows",
      "pid": 4242,
      "token": "133000000000000000"
    },
    "qmp": "/run/devvm-ab.sock",
    "ssh": [
      {
        "endpoint": "normal",
        "addr": "127.0.0.1:2222"
      }
    ],
    "serial_log": "/state/serial.log",
    "leftovers": [],
    "no_reboot": false
  }
}"#;

fn read_valid(dir: &Path) -> InstanceRecord {
    match InstanceRecord::read(dir).unwrap() {
        RecordRead::Valid(record) => *record,
        RecordRead::Absent => panic!("no record"),
        RecordRead::Corrupt(why) => panic!("corrupt: {why}"),
    }
}

#[skuld::test]
fn record_round_trips_every_field(#[fixture(unicode_home)] home: &Path) {
    let original = record(PathBuf::from("/work/project"));
    original.write(home).unwrap();
    assert_eq!(read_valid(home), original);

    let mut bare = original.clone();
    bare.expires = None;
    bare.machine = None;
    bare.network = Network::None;
    bare.guest = GuestKind::Windows;
    bare.arch = Arch::Arm64;
    bare.write(home).unwrap();
    assert_eq!(read_valid(home), bare);

    for guest in [GuestKind::Linux, GuestKind::Windows, GuestKind::Macos] {
        for arch in [Arch::Amd64, Arch::Arm64] {
            let mut each = original.clone();
            each.guest = guest;
            each.arch = arch;
            each.write(home).unwrap();
            assert_eq!(read_valid(home), each, "{guest:?} on {arch:?}");
        }
    }
}

/// `GOLDEN` with the top-level `field` set to `value`.
fn golden_with(field: &str, value: serde_json::Value) -> String {
    let mut json: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
    json[field] = value;
    json.to_string()
}

/// `GOLDEN` with `field` of its first layer set to `value`.
fn golden_with_layer(field: &str, value: &str) -> String {
    let mut json: serde_json::Value = serde_json::from_str(GOLDEN).unwrap();
    json["chain"][0][field] = value.into();
    json.to_string()
}

#[skuld::test]
fn a_record_with_a_field_that_means_nothing_is_corrupt_and_says_which(#[fixture(unicode_home)] home: &Path) {
    let cases = [
        ("an empty chain", golden_with("chain", serde_json::json!([])), "chain"),
        ("an uppercase name", golden_with("name", "FOO".into()), "lowercase"),
        ("an unknown guest", golden_with("guest", "beos".into()), "guest"),
        (
            "an unknown architecture",
            golden_with("arch", "mips".into()),
            "architecture",
        ),
        ("an unknown network", golden_with("network", "tun".into()), "network"),
        (
            "an unparseable creation time",
            golden_with("created", "yesterday".into()),
            "yesterday",
        ),
        ("an unparseable expiry", golden_with("expires", "soon".into()), "soon"),
        (
            "an uppercase id",
            golden_with("id", "000102030405060708090A0B0C0D0E0F".into()),
            "instance id",
        ),
        ("a layer key that is not hex", golden_with_layer("key", "zz"), "zz"),
        (
            "a layer generation that is not hex",
            golden_with_layer("generation", "zz"),
            "zz",
        ),
    ];
    for (what, text, expected) in cases {
        std::fs::write(home.join("record.json"), text).unwrap();
        match InstanceRecord::read(home).unwrap() {
            RecordRead::Corrupt(why) => assert!(why.contains(expected), "{what}: `{why}` does not say `{expected}`"),
            RecordRead::Absent => panic!("{what}: reported absent"),
            RecordRead::Valid(_) => panic!("{what}: accepted"),
        }
    }
}

#[skuld::test]
fn record_json_is_stable(#[fixture(unicode_home)] home: &Path) {
    record(PathBuf::from("/work/project")).write(home).unwrap();
    let written = std::fs::read_to_string(home.join("record.json")).unwrap();
    assert_eq!(written, GOLDEN);
    std::fs::write(home.join("record.json"), GOLDEN).unwrap();
    assert_eq!(read_valid(home), record(PathBuf::from("/work/project")));
}

#[skuld::test]
fn garbage_truncated_empty_and_wrong_typed_records_are_corrupt_not_errors(#[fixture(unicode_home)] home: &Path) {
    let wrong_type = GOLDEN.replace("\"cpus\": 2", "\"cpus\": \"two\"");
    let bad_name = GOLDEN.replace("\"name\": \"foo\"", "\"name\": \"../x\"");
    let bad_id = GOLDEN.replace("000102030405060708090a0b0c0d0e0f", "zz");
    let no_chain = GOLDEN.replace("\"chain\": [", "\"chain_\": [");
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("garbage", b"\x00\x01 not json".to_vec()),
        ("truncated", GOLDEN.as_bytes()[..GOLDEN.len() / 2].to_vec()),
        ("empty", Vec::new()),
        ("wrong type", wrong_type.into_bytes()),
        ("bad name", bad_name.into_bytes()),
        ("bad id", bad_id.into_bytes()),
        ("missing chain", no_chain.into_bytes()),
        (
            "schema zero",
            GOLDEN.replace("\"schema\": 1", "\"schema\": 0").into_bytes(),
        ),
        ("not utf-8", vec![0xff, 0xfe, 0xfd]),
    ];
    for (what, bytes) in cases {
        std::fs::write(home.join("record.json"), bytes).unwrap();
        match InstanceRecord::read(home).unwrap() {
            RecordRead::Corrupt(_) => {}
            RecordRead::Absent => panic!("{what}: reported absent"),
            RecordRead::Valid(_) => panic!("{what}: accepted"),
        }
    }
}

#[skuld::test]
fn an_absent_record_is_absent(#[fixture(unicode_home)] home: &Path) {
    assert!(matches!(InstanceRecord::read(home).unwrap(), RecordRead::Absent));
}

#[skuld::test]
fn a_newer_schema_is_a_store_error(#[fixture(unicode_home)] home: &Path) {
    // Whatever else it holds, a record from a newer devvm is recognized by its schema alone.
    std::fs::write(home.join("record.json"), r#"{"schema": 2, "something": ["new"]}"#).unwrap();
    let error = InstanceRecord::read(home).err().expect("a newer schema is refused");
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(error.message().contains("newer devvm"), "{}", error.message());
    assert!(error.message().contains("upgrade"), "{}", error.message());
}

#[skuld::test]
fn an_older_schema_is_refused_naming_it_and_the_schema_the_build_reads() {
    let path = Path::new("record.json");
    let Ok(Err(why)) = check_schema(path, 3, 5) else {
        panic!("an older schema is a corrupt record, not a store error")
    };
    assert!(why.contains("schema 3"), "{why}");
    assert!(why.contains("schema 5"), "{why}");
}

#[skuld::test]
fn a_newer_schema_is_refused_as_a_store_error_and_the_build_schema_is_read() {
    let path = Path::new("record.json");
    let error = check_schema(path, 6, 5).expect_err("a newer schema is refused");
    assert_eq!(error.kind(), ErrorKind::Store);
    assert!(matches!(check_schema(path, 5, 5), Ok(Ok(()))));
}

#[skuld::test]
fn write_replaces_atomically_while_a_reader_has_the_file_open(#[fixture(unicode_home)] home: &Path) {
    let first = record(PathBuf::from("/work/project"));
    first.write(home).unwrap();
    let held = std::fs::File::open(home.join("record.json")).unwrap();

    let mut second = first.clone();
    second.cpus = 8;
    second.write(home).unwrap();

    assert_eq!(read_valid(home).cpus, 8);
    drop(held);
    let leftovers: Vec<_> = std::fs::read_dir(home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        leftovers,
        vec![std::ffi::OsString::from("record.json")],
        "no temp file is left behind"
    );
}

#[skuld::test]
fn non_ascii_created_from_round_trips(#[fixture(unicode_home)] home: &Path) {
    let original = record(home.join("projects").join("Ж"));
    original.write(home).unwrap();
    assert_eq!(read_valid(home).created_from, home.join("projects").join("Ж"));
}

#[skuld::test]
fn a_non_unicode_created_from_is_refused_naming_the_path(#[fixture(unicode_home)] home: &Path) {
    let bad = PathBuf::from(non_unicode_name());
    let error = record(bad.clone()).write(home).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("not valid Unicode"), "{}", error.message());
    assert!(
        error.message().contains(&bad.to_string_lossy().into_owned()),
        "{}",
        error.message()
    );
    assert!(!home.join("record.json").exists());
}

#[skuld::test]
fn marker_round_trips_and_a_torn_marker_is_corrupt(#[fixture(unicode_home)] home: &Path) {
    assert!(matches!(read_marker(home).unwrap(), MarkerRead::Absent));
    for owner in [Owner::Process, Owner::None] {
        let marker = Marker { owner, id: id() };
        write_marker(home, &marker).unwrap();
        match read_marker(home).unwrap() {
            MarkerRead::Valid(read) => assert_eq!(read, marker),
            _ => panic!("the marker should read back"),
        }
    }
    let written = std::fs::read_to_string(home.join("owner.json")).unwrap();
    assert_eq!(
        written,
        r#"{"schema":1,"owner":"none","id":"000102030405060708090a0b0c0d0e0f"}"#
    );
    for torn in [
        &written[..written.len() / 2],
        "",
        r#"{"schema":1,"owner":"somebody","id":"00"}"#,
    ] {
        std::fs::write(home.join("owner.json"), torn).unwrap();
        assert!(matches!(read_marker(home).unwrap(), MarkerRead::Corrupt(_)), "{torn:?}");
    }
}

#[skuld::test]
fn a_marker_from_a_newer_devvm_is_a_store_error(#[fixture(unicode_home)] home: &Path) {
    std::fs::write(home.join("owner.json"), r#"{"schema":2,"owner":"cluster"}"#).unwrap();
    let error = read_marker(home).err().expect("a newer schema is refused");
    assert_eq!(error.kind(), ErrorKind::Store);
}

#[skuld::test]
fn instance_ids_are_random_and_round_trip_as_text() {
    let first = InstanceId::random();
    assert_ne!(first, InstanceId::random());
    assert_eq!(first.to_string().len(), 32);
    assert_eq!(first.to_string().parse::<InstanceId>().unwrap(), first);
    assert!("xyz".parse::<InstanceId>().is_err());
    assert!(
        "000102030405060708090A0B0C0D0E0F".parse::<InstanceId>().is_err(),
        "ids are lowercase"
    );
}

#[skuld::test]
fn a_holder_round_trips_and_goes_when_removed(#[fixture(unicode_home)] home: &Path) {
    assert!(read_holder(home).unwrap().is_none());
    let holder = Holder {
        process: machine().process,
        program: "devvm".to_owned(),
    };
    write_holder(home, &holder).unwrap();
    assert_eq!(read_holder(home).unwrap(), Some(holder));
    remove_holder(home).unwrap();
    assert!(read_holder(home).unwrap().is_none());
    remove_holder(home).expect("a record that is not there is as good as removed");
}

#[skuld::test]
fn a_torn_holder_reads_as_none_and_one_from_a_newer_devvm_is_an_error(#[fixture(unicode_home)] home: &Path) {
    for torn in ["", "{ torn", r#"{"schema":1,"program":"devvm"}"#, r#"{"schema":0}"#] {
        std::fs::write(home.join(HOLDER_FILE), torn).unwrap();
        assert!(read_holder(home).unwrap().is_none(), "{torn:?}");
    }
    std::fs::write(home.join(HOLDER_FILE), r#"{"schema":2}"#).unwrap();
    assert_eq!(read_holder(home).unwrap_err().kind(), ErrorKind::Store);
}

#[skuld::test]
fn the_program_is_the_file_name_without_its_directory_and_its_exe_extension() {
    let at = |name: &str| program_name(&Path::new("bin").join(name));
    assert_eq!(at("devvm.exe").as_deref(), Some("devvm"));
    assert_eq!(at("DEVVM.EXE").as_deref(), Some("DEVVM"));
    assert_eq!(at("devvm").as_deref(), Some("devvm"));
    assert_eq!(at("my.tool.exe").as_deref(), Some("my.tool"));
}

#[skuld::test]
fn a_program_name_keeps_the_dots_that_are_not_an_exe_extension() {
    assert_eq!(program_name(Path::new("devvm-0.1.0")).as_deref(), Some("devvm-0.1.0"));
    assert_eq!(program_name(Path::new("my.tool")).as_deref(), Some("my.tool"));
}

/// Linux appends ` (deleted)` to the path of an executable replaced while it runs.
#[skuld::test]
fn a_program_name_drops_the_deleted_mark_linux_appends_to_a_replaced_executable() {
    let at = |name: &str| program_name(&Path::new("bin").join(name));
    assert_eq!(at("devvm (deleted)").as_deref(), Some("devvm"));
    assert_eq!(at("devvm.exe (deleted)").as_deref(), Some("devvm"));
    assert_eq!(at("devvm (deleted) (deleted)").as_deref(), Some("devvm (deleted)"));
    assert_eq!(at("devvm (deleted)x").as_deref(), Some("devvm (deleted)x"));
    assert_eq!(at(" (deleted)").as_deref(), Some(" (deleted)"));
}

#[skuld::test]
fn a_path_without_a_file_name_names_no_program() {
    assert_eq!(program_name(Path::new("")), None);
    assert_eq!(program_name(Path::new("..")), None);
}
