//! The command line: every 0.1.0 command and its flags.

use std::path::PathBuf;
use std::str::FromStr;

use clap::{Args, Parser, Subcommand};
use devvm::{Arch, Network};

#[cfg(test)]
#[path = "args_tests.rs"]
mod args_tests;

#[derive(Parser)]
#[command(name = "devvm", version, about = "Disposable virtual machines for tests")]
pub(crate) struct Cli {
    /// Write a JSON description of how the command ended to this file (also: DEVVM_STATUS_FILE).
    #[arg(long, global = true, value_name = "PATH", env = "DEVVM_STATUS_FILE")]
    pub(crate) status_file: Option<PathBuf>,

    /// Also show step output and debug notes (also: DEVVM_VERBOSE).
    #[arg(short, long, global = true)]
    pub(crate) verbose: bool,

    #[command(subcommand)]
    pub(crate) command: Cmd,
}

/// What a new VM gets.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct VmFlags {
    /// Guest architecture: amd64 or arm64 (default: the host's). The other architecture runs under emulation.
    #[arg(long, value_parser = parse_arch)]
    pub arch: Option<Arch>,
    /// Number of CPUs (default: 2).
    #[arg(long)]
    pub cpus: Option<u32>,
    /// Memory, for example 4GiB or 512MiB (default: 4GiB).
    #[arg(long, value_parser = parse_memory)]
    pub memory: Option<u64>,
    /// `nat` gives the guest internet access; `none` cuts it off.
    #[arg(long, value_parser = parse_network, default_value = "nat")]
    pub network: Network,
}

/// `run` and `ssh -- <command>` go through the guest's login shell.
macro_rules! login_help {
    () => {
        "The command runs through the guest account's login shell, so its startup files run first. They are the \
user's: one that redefines `cd` or `exec`, or sets `cdable_vars`, changes the command. devvm does not check."
    };
}

/// `run` and `ssh -- <command>` take an argument vector.
macro_rules! argv_help {
    () => {
        "The words after `--` are one program and its arguments, not a string for a shell: `-- 'a; b'` looks for a \
program named `a; b` and exits 127. For a pipeline, `~` or `$HOME`, run a shell: `-- sh -c 'a; b'` in a Linux guest, \
`-- pwsh -c 'a; b'` in a Windows guest."
    };
}

/// The commands that take a recipe start a Linux guest from a disk image if the recipe says `url` and a digest.
macro_rules! image_help {
    () => {
        "`from: { url, sha256 }` or `from: { url, sha512 }`, or `amd64:` and `arm64:` each with one of those, starts a \
Linux guest from a disk image. Each `url` (http or https) takes exactly one of `sha256` (64 hex digits) and `sha512` (128 hex digits). \
The image must boot with UEFI from a virtio disk, run cloud-init with the NoCloud datasource, run sshd with SFTP on \
port 22 that accepts ed25519 keys, have bash, sudo, tar, sh, sed, sync (with `-f`), cat, mkdir and rm, and power off on a power-button press. Details: \
https://github.com/bindreams/devvm#disk-images"
    };
}

macro_rules! snapshot_help {
    () => {
        "`copy: { from: git-tracked }` copies the files git tracks as they are on disk. A project that changes during \
the copy can give a mixed copy. The names in it follow the rule of `devvm cp --help`."
    };
}

/// One side of a `cp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CpSide {
    Local(PathBuf),
    Guest { instance: String, path: String },
}

/// The commands this build understands.
#[derive(Clone, Debug, PartialEq, Eq, Subcommand)]
pub enum Cmd {
    /// Boot a VM, run a command in it, destroy the VM; exits with the command's exit code.
    #[command(after_help = concat!(argv_help!(), "\n\n", login_help!(), "\n\n", snapshot_help!(), "\n\n", image_help!()))]
    Run {
        /// A recipe file (`*.devvm.yaml`) or the name of an embedded base recipe.
        reference: String,
        #[command(flatten)]
        flags: VmFlags,
        /// Run as root (Linux, macOS) or with the administrator token (Windows).
        #[arg(long)]
        elevated: bool,
        /// The command and its arguments, after `--`.
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Start an instance that stays until `rm`.
    #[command(after_help = concat!(snapshot_help!(), "\n\n", image_help!()))]
    Up {
        /// A recipe file (`*.devvm.yaml`) or the name of an embedded base recipe.
        reference: String,
        /// The instance's name: letters, digits and `-`.
        #[arg(long)]
        name: String,
        #[command(flatten)]
        flags: VmFlags,
    },
    /// Open a shell in an instance, or run one command (after `--`).
    #[command(after_help = concat!(argv_help!(), "\n\n", login_help!()))]
    Ssh {
        instance: String,
        /// Run as root (Linux, macOS) or with the administrator token (Windows). Needs a command.
        #[arg(long, requires = "argv")]
        elevated: bool,
        /// The command and its arguments, after `--`; none opens an interactive shell.
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Copy files or directories between the host and an instance.
    #[command(after_help = CP_HELP)]
    Cp {
        #[arg(value_parser = cp_side)]
        src: CpSide,
        #[arg(value_parser = cp_side)]
        dst: CpSide,
    },
    /// List the instances created from this directory.
    Ls {
        /// List the instances created from any directory.
        #[arg(long)]
        all: bool,
        /// One JSON object per line.
        #[arg(long)]
        json: bool,
    },
    /// Shut an instance down; its disk is kept.
    Stop { name: String },
    /// Start a stopped instance.
    Start { name: String },
    /// Kill an instance and delete it.
    Rm { name: String },
    /// Build the image a recipe describes without starting it.
    #[command(after_help = concat!(snapshot_help!(), "\n\n", image_help!()))]
    Build {
        /// A recipe file (`*.devvm.yaml`) or the name of an embedded base recipe.
        reference: String,
        #[arg(long, value_parser = parse_arch)]
        arch: Option<Arch>,
    },
    /// Check this host: accelerator, QEMU, disk space.
    Doctor,
}

const CP_HELP: &str = "\
A guest path is INSTANCE:PATH. A path of one letter, a colon and a slash (c:\\dir or c:/dir) is a local Windows drive \
path, as in scp. An instance named with one letter is still addressable as c:path: without a slash directly after \
the colon, `c:path` names the file `path` in the instance `c`.

The copied trees must not hold names that differ only in case, names ending in a dot or a space, or Windows device \
names (`nul`, `con`). devvm does not check.

No permission is carried but the execute bit. A file reaches the guest as 0755 if the host file has any execute bit and \
as 0644 otherwise, less the guest account's umask. A file copied out gets the host's default mode, plus execute for \
whoever can read it if the guest file has any execute bit. Directories get default modes, and a file that replaces \
another takes the new mode. A private file is readable by others on the other side: restrict it again after the copy. \
A Windows host has no execute bit, so a file copied from it is not executable in a Linux guest: make it executable \
afterwards with devvm ssh <name> -- chmod +x <file>. A file copied out to a Windows host loses its execute bit.

A file time outside 1970 to 2106 is not kept, and `cp` says so; for a file in the guest the range is 1970 to 2038, \
because SFTP version 3 carries 32 bits.

An interrupted copy can leave `.devvm-partial-*` files beside the destination.";

fn parse_arch(text: &str) -> Result<Arch, String> {
    match text {
        "amd64" => Ok(Arch::Amd64),
        "arm64" => Ok(Arch::Arm64),
        other => Err(format!("`{other}` is not an architecture (expected amd64 or arm64)")),
    }
}

fn parse_memory(text: &str) -> Result<u64, String> {
    bytesize::ByteSize::from_str(text)
        .map(|size| size.as_u64())
        .map_err(|e| format!("`{text}` is not a memory size such as 4GiB or 512MiB: {e}"))
}

fn parse_network(text: &str) -> Result<Network, String> {
    match text {
        "nat" => Ok(Network::Nat),
        "none" => Ok(Network::None),
        other => Err(format!("`{other}` is not a network mode (expected nat or none)")),
    }
}

fn cp_side(text: &str) -> Result<CpSide, std::convert::Infallible> {
    Ok(parse_cp_side(text))
}

/// Splits a `cp` argument into a local path or `INSTANCE:PATH`.
///
/// The rule is scp's. Text without a colon is a local path. A colon after a path separator belongs to the file name.
/// A single ASCII letter, a colon, and then a slash, a backslash or nothing is a local Windows drive path. Everything
/// else before the first colon names an instance.
pub fn parse_cp_side(text: &str) -> CpSide {
    let local = || CpSide::Local(PathBuf::from(text));
    let Some((head, rest)) = text.split_once(':') else {
        return local();
    };
    if head.is_empty() || head.contains(['/', '\\']) {
        return local();
    }
    let is_drive = head.len() == 1 && head.as_bytes()[0].is_ascii_alphabetic();
    if is_drive && (rest.is_empty() || rest.starts_with(['/', '\\'])) {
        return local();
    }
    CpSide::Guest {
        instance: head.to_owned(),
        path: rest.to_owned(),
    }
}
