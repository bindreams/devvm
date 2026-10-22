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
    Ssh {
        instance: String,
        /// Run as root (Linux, macOS) or with the administrator token (Windows).
        #[arg(long)]
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
the colon, `c:path` names the file `path` in the instance `c`.";

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
