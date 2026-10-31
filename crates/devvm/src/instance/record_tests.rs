use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use cosca::identity::{Platform, ProcessIdRecord};

use crate::backend::{Endpoint, MachineRecord, SshEndpoint};
use crate::instance::name::InstanceName;
use crate::instance::record::{
    InstanceId, InstanceRecord, Marker, MarkerRead, Owner, RecordRead, read_marker, write_marker,
};
use crate::recipe::hash::LayerKey;
use crate::store::layers::{Generation, LayerRef};
use crate::test_support::fixtures::unicode_home;
use crate::{Arch, ErrorKind, GuestKind, Network};

fn id() -> InstanceId {
    "000102030405060708090a0b0c0d0e0f".parse().unwrap()
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
            version: 1,
            platform: Platform::Windows,
            pid: 4242,
            token: 133_000_000_000_000_000,
            boot_id: None,
            pid_ns: None,
        },
        qmp: "/run/devvm-ab.sock".to_owned(),
        serial_log: std::path::PathBuf::from("/state/serial.log"),
        leftovers: Vec::new(),
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
      "v": 1,
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

#[cfg(windows)]
fn non_unicode_path() -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_wide(&[0x43, 0x3a, 0x5c, 0xd800, 0x78]))
}

#[cfg(unix)]
fn non_unicode_path() -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(b"/work/\xff\xfe"))
}

#[skuld::test]
fn a_non_unicode_created_from_is_refused_naming_the_path(#[fixture(unicode_home)] home: &Path) {
    let bad = non_unicode_path();
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
