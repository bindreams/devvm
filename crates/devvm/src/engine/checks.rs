//! What `devvm doctor` checks, and what `availability` is made of.
//!
//! The engine states the host's own pieces (the accelerator, room to work in, the tools devvm runs) and the backend
//! states its own, so no QEMU name appears here.

use super::Engine;
use crate::backend::Check;
use crate::host::Host;
use crate::{Arch, Dirs, Unavailable};

#[cfg(test)]
#[path = "checks_tests.rs"]
mod checks_tests;

impl Engine {
    /// Every line of `devvm doctor`, in the order to print them: the host's accelerator, the free space of the data and
    /// the cache directory, the `ssh` and `git` that devvm runs, then the backend's own. A line that fails does not stop
    /// the others. Used by `devvm-cli`; not a documented API.
    #[doc(hidden)]
    pub fn checks(&self) -> Vec<Check> {
        let mut checks = host_checks(self.host, &self.dirs);
        checks.push(tool_check(self.host, "ssh", &["-V"]));
        checks.push(tool_check(self.host, "git", &["--version"]));
        checks.extend(self.backend_checks());
        checks
    }

    /// Whether this host can run VMs; what is missing otherwise. The first failing line among the accelerator and the
    /// backend's. Free space and the tools are `doctor` lines only.
    pub fn availability(&self) -> std::result::Result<(), Unavailable> {
        std::iter::once(accelerator_check(self.host))
            .chain(self.backend_checks())
            .find_map(|check| check.outcome.err())
            .map_or(Ok(()), |missing| Err(Unavailable { missing }))
    }

    /// The backend's lines, or the one line that says there is no backend.
    fn backend_checks(&self) -> Vec<Check> {
        match self.backend() {
            Ok(backend) => backend.checks(),
            Err(e) => vec![Check {
                name: "backend".to_owned(),
                outcome: Err(e.message().to_owned()),
            }],
        }
    }
}

/// The accelerator and the free space of the data and the cache directory: what `host` answers.
fn host_checks(host: &dyn Host, dirs: &Dirs) -> Vec<Check> {
    vec![
        accelerator_check(host),
        space_check(host, "data", &dirs.data),
        space_check(host, "cache", &dirs.cache),
    ]
}

fn accelerator_check(host: &dyn Host) -> Check {
    Check {
        name: "accelerator".to_owned(),
        outcome: match host.accelerator(Arch::host()) {
            Ok(accel) => Ok(format!("{accel:?}").to_lowercase()),
            Err(missing) => Err(missing.missing),
        },
    }
}

/// The free space in `dir`; a directory that is not there yet is asked through its nearest ancestor.
fn space_check(host: &dyn Host, what: &str, dir: &std::path::Path) -> Check {
    Check {
        name: format!("free space ({what})"),
        outcome: match host.free_space(dir) {
            Ok(bytes) => Ok(format!("{} free in {}", bytesize::ByteSize(bytes), dir.display())),
            Err(e) => Err(format!("cannot tell the free space in {}: {e}", dir.display())),
        },
    }
}

/// Runs `program` with `args` and says what it printed first, on stdout or else on stderr (`ssh -V` writes there).
pub(super) fn tool_check(host: &dyn Host, program: &str, args: &[&str]) -> Check {
    let outcome = (|| {
        let mut command = std::process::Command::new(program);
        command.args(args).stdin(std::process::Stdio::null());
        host.quiet(&mut command);
        let output = command
            .output()
            .map_err(|e| format!("cannot run `{program}` (is it installed and on PATH?): {e}"))?;
        if !output.status.success() {
            return Err(format!("`{program} {}` failed ({})", args.join(" "), output.status));
        }
        let text = [&output.stdout, &output.stderr]
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(bytes))
            .find_map(|text| {
                text.lines()
                    .next()
                    .map(|line| line.trim().to_owned())
                    .filter(|line| !line.is_empty())
            });
        Ok(text.unwrap_or_else(|| "found".to_owned()))
    })();
    Check {
        name: program.to_owned(),
        outcome,
    }
}
