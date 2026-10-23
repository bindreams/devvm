use std::path::PathBuf;

use clap::Parser;
use devvm::{Arch, Network};

use crate::args::{Cli, Cmd, CpSide, VmFlags, parse_cp_side};

fn parse(args: &[&str]) -> Cmd {
    let full = std::iter::once("devvm").chain(args.iter().copied());
    Cli::try_parse_from(full)
        .unwrap_or_else(|e| panic!("{args:?}: {e}"))
        .command
}

fn flags(arch: Option<Arch>, cpus: Option<u32>, memory: Option<u64>, network: Network) -> VmFlags {
    VmFlags {
        arch,
        cpus,
        memory,
        network,
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[skuld::test]
fn every_command_parses() {
    let none = flags(None, None, None, Network::Nat);
    let table: Vec<(Vec<&str>, Cmd)> = vec![
        (
            vec!["run", "ubuntu-24.04", "--", "uname", "-a"],
            Cmd::Run {
                reference: "ubuntu-24.04".into(),
                flags: none.clone(),
                elevated: false,
                argv: strings(&["uname", "-a"]),
            },
        ),
        (
            vec![
                "run",
                "r.devvm.yaml",
                "--arch",
                "arm64",
                "--cpus",
                "4",
                "--memory",
                "4GiB",
                "--network",
                "none",
                "--elevated",
                "--",
                "id",
                "-u",
            ],
            Cmd::Run {
                reference: "r.devvm.yaml".into(),
                flags: flags(Some(Arch::Arm64), Some(4), Some(4 << 30), Network::None),
                elevated: true,
                argv: strings(&["id", "-u"]),
            },
        ),
        (
            vec!["run", "x", "--memory", "512MiB", "--", "true"],
            Cmd::Run {
                reference: "x".into(),
                flags: flags(None, None, Some(512 << 20), Network::Nat),
                elevated: false,
                argv: strings(&["true"]),
            },
        ),
        (
            vec!["run", "x", "--", "echo", "--", "--elevated", "-x", "--network", "none"],
            Cmd::Run {
                reference: "x".into(),
                flags: none.clone(),
                elevated: false,
                argv: strings(&["echo", "--", "--elevated", "-x", "--network", "none"]),
            },
        ),
        (
            vec!["up", "x", "--name", "box", "--cpus", "1"],
            Cmd::Up {
                reference: "x".into(),
                name: "box".into(),
                flags: flags(None, Some(1), None, Network::Nat),
            },
        ),
        (
            vec!["ssh", "box"],
            Cmd::Ssh {
                instance: "box".into(),
                elevated: false,
                argv: Vec::new(),
            },
        ),
        (
            vec!["ssh", "box", "--elevated", "--", "whoami"],
            Cmd::Ssh {
                instance: "box".into(),
                elevated: true,
                argv: strings(&["whoami"]),
            },
        ),
        (
            vec!["cp", "a.txt", "box:~/a.txt"],
            Cmd::Cp {
                src: CpSide::Local(PathBuf::from("a.txt")),
                dst: CpSide::Guest {
                    instance: "box".into(),
                    path: "~/a.txt".into(),
                },
            },
        ),
        (
            vec!["ls"],
            Cmd::Ls {
                all: false,
                json: false,
            },
        ),
        (vec!["ls", "--all", "--json"], Cmd::Ls { all: true, json: true }),
        (vec!["stop", "box"], Cmd::Stop { name: "box".into() }),
        (vec!["start", "box"], Cmd::Start { name: "box".into() }),
        (vec!["rm", "box"], Cmd::Rm { name: "box".into() }),
        (
            vec!["build", "x"],
            Cmd::Build {
                reference: "x".into(),
                arch: None,
            },
        ),
        (
            vec!["build", "x", "--arch", "amd64"],
            Cmd::Build {
                reference: "x".into(),
                arch: Some(Arch::Amd64),
            },
        ),
        (vec!["doctor"], Cmd::Doctor),
    ];
    for (args, expected) in table {
        assert_eq!(parse(&args), expected, "{args:?}");
    }
}

#[skuld::test]
fn the_status_file_flag_is_global() {
    let cli = Cli::try_parse_from(["devvm", "ls", "--status-file", "s.json"]).unwrap();
    assert_eq!(cli.status_file, Some(PathBuf::from("s.json")));
}

#[skuld::test]
fn bad_command_lines_are_usage_errors() {
    for args in [
        vec!["run", "x"],
        vec!["run", "x", "--cpus", "many", "--", "true"],
        vec!["run", "x", "--memory", "lots", "--", "true"],
        vec!["run", "x", "--network", "wifi", "--", "true"],
        vec!["run", "x", "--arch", "riscv", "--", "true"],
        vec!["up", "x"],
        vec!["rm"],
        vec!["bogus"],
        vec!["cp", "only-one"],
    ] {
        let full = std::iter::once("devvm").chain(args.iter().copied());
        assert!(Cli::try_parse_from(full).is_err(), "{args:?}");
    }
}

#[skuld::test]
fn cp_sides() {
    let local = |text: &str| CpSide::Local(PathBuf::from(text));
    let guest = |instance: &str, path: &str| CpSide::Guest {
        instance: instance.to_owned(),
        path: path.to_owned(),
    };
    let table = [
        (r"c:\x", local(r"c:\x")),
        ("c:/x", local("c:/x")),
        ("C:", local("C:")),
        (r"C:\", local(r"C:\")),
        ("cc:/x", guest("cc", "/x")),
        ("c:x", guest("c", "x")),
        ("vm:~/a", guest("vm", "~/a")),
        ("vm:", guest("vm", "")),
        (r"\\host\share\file", local(r"\\host\share\file")),
        ("plain.txt", local("plain.txt")),
        ("./a:b", local("./a:b")),
        (r"dir\a:b", local(r"dir\a:b")),
        (":x", local(":x")),
        (r"box:C:\Users\user\a", guest("box", r"C:\Users\user\a")),
        ("box:path with spaces/Ж", guest("box", "path with spaces/Ж")),
        ("/home/Anna Ж/file", local("/home/Anna Ж/file")),
    ];
    for (text, expected) in table {
        assert_eq!(parse_cp_side(text), expected, "{text}");
    }
}

#[skuld::test]
fn cp_help_says_a_one_letter_instance_is_addressable_as_c_colon_path() {
    let help = Cli::try_parse_from(["devvm", "cp", "--help"])
        .err()
        .unwrap()
        .to_string();
    assert!(help.contains("c:path"), "{help}");
    assert!(help.contains(r"c:\dir"), "{help}");
}
