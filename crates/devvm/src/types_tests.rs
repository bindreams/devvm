use std::str::FromStr;

use crate::types::{Arch, Command, ContentHash, Network, RunAs, Sha256, Sha512, StartOptions};

#[skuld::test]
fn target_triple_matches_this_build() {
    assert!(crate::TARGET.starts_with(std::env::consts::ARCH), "{}", crate::TARGET);
}

const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

#[skuld::test]
fn sha256_parses_either_case_and_displays_lowercase() {
    let lower = Sha256::from_str(HEX).unwrap();
    let upper = Sha256::from_str(&HEX.to_uppercase()).unwrap();
    assert_eq!(lower, upper);
    assert_eq!(upper.to_string(), HEX);
    assert_eq!(lower.as_bytes()[1], 0x11);
    assert_eq!(Sha256::from_bytes(*lower.as_bytes()), lower);
}

#[skuld::test]
fn sha256_rejects_wrong_length_and_non_hex() {
    assert!(Sha256::from_str("").is_err());
    assert!(Sha256::from_str(&HEX[..62]).is_err());
    assert!(Sha256::from_str(&format!("{HEX}00")).is_err());
    assert!(Sha256::from_str(&format!("{}g", &HEX[..63])).is_err());
    assert!(Sha256::from_str(&format!(" {}", &HEX[..63])).is_err());
}

#[skuld::test]
fn sha256_serde_is_a_hex_string() {
    let sha = Sha256::from_str(HEX).unwrap();
    let json = serde_json::to_string(&sha).unwrap();
    assert_eq!(json, format!("\"{HEX}\""));
    assert_eq!(serde_json::from_str::<Sha256>(&json).unwrap(), sha);
    assert!(serde_json::from_str::<Sha256>("\"abc\"").is_err());
}

const HEX512: &str = concat!(
    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
);

#[skuld::test]
fn sha512_parses_either_case_and_displays_lowercase() {
    let lower = Sha512::from_str(HEX512).unwrap();
    let upper = Sha512::from_str(&HEX512.to_uppercase()).unwrap();
    assert_eq!(lower, upper);
    assert_eq!(upper.to_string(), HEX512);
    assert_eq!(lower.as_bytes()[1], 0x11);
    assert_eq!(Sha512::from_bytes(*lower.as_bytes()), lower);
}

#[skuld::test]
fn sha512_rejects_wrong_length_and_non_hex() {
    assert!(Sha512::from_str("").is_err());
    assert!(Sha512::from_str(HEX).is_err(), "a sha256 is not a sha512");
    assert!(Sha512::from_str(&HEX512[..126]).is_err());
    assert!(Sha512::from_str(&format!("{HEX512}00")).is_err());
    assert!(Sha512::from_str(&format!("{}g", &HEX512[..127])).is_err());
    assert!(Sha256::from_str(HEX512).is_err(), "a sha512 is not a sha256");
}

#[skuld::test]
fn sha512_serde_is_a_hex_string() {
    let sha = Sha512::from_str(HEX512).unwrap();
    let json = serde_json::to_string(&sha).unwrap();
    assert_eq!(json, format!("\"{HEX512}\""));
    assert_eq!(serde_json::from_str::<Sha512>(&json).unwrap(), sha);
    assert!(serde_json::from_str::<Sha512>("\"abc\"").is_err());
}

#[skuld::test]
fn a_content_hash_names_its_algorithm_and_displays_its_hex() {
    let short = ContentHash::from(Sha256::from_str(HEX).unwrap());
    let long = ContentHash::from(Sha512::from_str(HEX512).unwrap());
    assert_eq!((short.algorithm(), short.to_string()), ("sha256", HEX.to_owned()));
    assert_eq!((long.algorithm(), long.to_string()), ("sha512", HEX512.to_owned()));
    assert_ne!(short, long);
}

#[skuld::test]
fn arch_host_matches_consts_arch() {
    let expected = match std::env::consts::ARCH {
        "x86_64" => Arch::Amd64,
        "aarch64" => Arch::Arm64,
        other => panic!("unsupported test host arch {other}"),
    };
    assert_eq!(Arch::host(), expected);
    assert_eq!(Arch::Amd64.as_str(), "amd64");
    assert_eq!(Arch::Arm64.as_str(), "arm64");
}

#[skuld::test]
fn start_options_default_is_nat_and_unset() {
    let options = StartOptions::default();
    assert_eq!(options.network, Network::Nat);
    assert_eq!((options.cpus, options.memory), (None, None));
}

#[skuld::test]
fn command_new_collects_argv_with_user_defaults() {
    let command = Command::new(["echo", "a b"]);
    assert_eq!(command.argv, ["echo", "a b"]);
    assert_eq!(command.run_as, RunAs::User);
    assert!(command.cwd.is_none());
}
