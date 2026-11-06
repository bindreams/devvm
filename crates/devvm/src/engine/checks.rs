//! What `devvm doctor` checks, and what `availability` is made of.
//!
//! The engine states the host's own pieces (the accelerator, room to work in, the tools devvm runs) and the backend
//! states its own, so no QEMU name appears here.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use super::Engine;
use crate::backend::{Check, Detail};
use crate::copy::Git;
use crate::exit::Progress;
use crate::host::Host;
use crate::store::fetch::Transport;
use crate::{Arch, Dirs, Result, Unavailable};

#[cfg(test)]
#[path = "checks_tests.rs"]
mod checks_tests;

impl Engine {
    /// Every line of `devvm doctor` for this process's environment, in the order to print them. Builds the engine as
    /// [`Engine::from_env`] does, reaping included, but does not fail on an unreadable `config.yaml`: the QEMU line
    /// says so instead. The one `Err` left is `Dirs::from_env`, before any line exists. Used by `devvm-cli`; not a
    /// documented API.
    #[doc(hidden)]
    pub fn doctor_checks(progress: Arc<dyn Progress>, transport: Arc<dyn Transport>) -> Result<Vec<Check>> {
        Engine::build_from_env(progress, transport).map(|(engine, _)| engine.checks())
    }

    /// Every line of `devvm doctor`, in the order to print them: the host's accelerator, the free space of the data, the
    /// cache and the state directory, the `ssh` and `git` that devvm runs, then the backend's own. A line that fails
    /// does not stop the others.
    pub(crate) fn checks(&self) -> Vec<Check> {
        let mut checks = host_checks(self.host, &self.dirs);
        checks.push(ssh_check(self.host));
        checks.push(git_check(self.host, self.progress.as_ref()));
        checks.extend(self.backend_checks(Detail::Full));
        checks
    }

    /// Whether this host can run VMs; what is missing otherwise. The first failing line among the accelerator and the
    /// backend's, with the backend asked only when the accelerator passes, and only [`Detail::Brief`]. Free space and
    /// the tools are `doctor` lines only.
    pub fn availability(&self) -> std::result::Result<(), Unavailable> {
        accelerator_check(self.host)
            .outcome
            .err()
            .or_else(|| {
                self.backend_checks(Detail::Brief)
                    .into_iter()
                    .find_map(|check| check.outcome.err())
            })
            .map_or(Ok(()), |missing| Err(Unavailable { missing }))
    }

    /// The backend's lines, or the one line that says there is no backend.
    fn backend_checks(&self, detail: Detail) -> Vec<Check> {
        match self.backend() {
            Ok(backend) => backend.checks(detail),
            Err(e) => vec![Check {
                name: "backend".to_owned(),
                outcome: Err(e.message().to_owned()),
            }],
        }
    }
}

/// The accelerator and the free space of the data, the cache and the state directory: what `host` answers.
fn host_checks(host: &dyn Host, dirs: &Dirs) -> Vec<Check> {
    vec![
        accelerator_check(host),
        space_check(host, "data", &dirs.data),
        space_check(host, "cache", &dirs.cache),
        space_check(host, "state", &dirs.state),
    ]
}

fn accelerator_check(host: &dyn Host) -> Check {
    Check {
        name: "accelerator".to_owned(),
        outcome: match host.accelerator(Arch::host()) {
            Ok(accel) => Ok(accel.name().to_owned()),
            Err(missing) => Err(missing.missing),
        },
    }
}

/// The free space in `dir`; a directory that is not there yet is asked through its nearest ancestor.
fn space_check(host: &dyn Host, what: &str, dir: &Path) -> Check {
    Check {
        name: format!("free space ({what})"),
        outcome: match host.free_space(dir) {
            Ok(bytes) => Ok(format!("{} free in {}", bytesize::ByteSize(bytes), dir.display())),
            Err(e) => Err(format!("cannot tell the free space in {}: {e}", dir.display())),
        },
    }
}

/// `ssh -V`, run as devvm runs `ssh`.
fn ssh_check(host: &dyn Host) -> Check {
    let mut command = Command::new("ssh");
    command.arg("-V").stdin(Stdio::null());
    host.quiet(&mut command);
    tool_check("ssh", command)
}

/// `git --version`, run as a snapshot runs `git`: in the same scrubbed environment.
fn git_check(host: &dyn Host, progress: &dyn Progress) -> Check {
    let mut command = Git::from_process(host, progress).command(Path::new("."));
    command.arg("--version").stdin(Stdio::null());
    tool_check("git", command)
}

/// Runs `command` and says what it printed first, on stdout or else on stderr (`ssh -V` writes there).
pub(super) fn tool_check(program: &str, mut command: Command) -> Check {
    let outcome = (|| {
        let output = command
            .output()
            .map_err(|e| format!("cannot run `{program}` (is it installed and on PATH?): {e}"))?;
        if !output.status.success() {
            let args: Vec<_> = command.get_args().map(|arg| arg.to_string_lossy()).collect();
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
