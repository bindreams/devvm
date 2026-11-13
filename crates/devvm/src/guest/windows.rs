//! The Windows guest: commands run as PowerShell 7 through ssh-broker's agent in the user's session, an elevated one at
//! High through a scheduled-task helper, and the per-instance seed is a read-only USB disk the boot task applies.
//!
//! The base image's bootstrap, the boot task and the helper's runner are the base builder's (the templates it embeds);
//! this is what the host knows of the guest they make.

use crate::access::SshSession;
use crate::backend::{DeviceProfile, DiskBus, MediaKind, NetModel, Video};
use crate::guest::{GuestOs, Readiness, SEED_IMAGE, SeedMedia, SeedSpec};
use crate::host::{Host, names};
use crate::media::write_cidata_image;
use crate::recipe::{Setting, Shell, UacMode};
use crate::{Command, Error, ErrorKind, Result, RunAs};

pub(crate) mod builder;
mod cmdquote;
pub(crate) mod elevate;
mod expiry;
mod extract;
mod paths;
mod pins;
pub(crate) mod ready;
mod seed;
mod templates;
pub(crate) mod wrap;

#[cfg(test)]
#[path = "windows_tests.rs"]
mod windows_tests;

/// The first statement of every PowerShell that devvm starts itself: the console's output encoding is the OEM code
/// page otherwise, and non-ASCII text a program prints through it comes out as `?`. The user's script follows as a
/// script block of its own, so that a first statement that must come first (`using`, `param`) still does.
const UTF8_OUTPUT: &str = "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)";

/// The boot task's fresh random id of this boot, written before sshd starts.
const BOOT_ID_FILE: &str = r"C:\ProgramData\devvm\boot-id";

/// UAC's policy key; `ConsentPromptBehaviorAdmin = 0` elevates an administrator without a prompt, and applies at once.
const UAC_POLICY: &str = r"HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";

/// A Windows guest reached as `user` over SSH.
pub(crate) struct WindowsGuest;

impl GuestOs for WindowsGuest {
    fn instance_seed(&self, host: &dyn Host, spec: &SeedSpec) -> Result<SeedMedia> {
        let media = write_cidata_image(
            host,
            &seed::seed_files(spec)?,
            MediaKind::UsbDisk,
            true,
            0,
            &spec.dir.join(SEED_IMAGE),
        )?;
        Ok(SeedMedia { media })
    }

    fn boot_id_command(&self) -> Command {
        Command::new(["cmd", "/d", "/c", &format!("type {BOOT_ID_FILE}")])
    }

    /// `Write-VolumeCache` works at Medium integrity, so the flush needs no elevation.
    fn flush_command(&self) -> Command {
        Command {
            argv: self
                .shell_argv(Some(Shell::Pwsh), "Write-VolumeCache -DriveLetter C")
                .expect("PowerShell is a shell of a Windows guest"),
            run_as: RunAs::User,
            cwd: None,
        }
    }

    fn wrap(&self, cmd: &Command) -> Result<String> {
        wrap::wrap(cmd)
    }

    fn sftp_path(&self, guest_path: &str, home: &str) -> String {
        paths::sftp_path(guest_path, home)
    }

    fn user_path(&self, sftp_path: &str) -> String {
        paths::user_path(sftp_path)
    }

    fn split_root<'a>(&self, path: &'a str) -> Result<(String, &'a str)> {
        paths::split_root(path)
    }

    /// Bumped when command semantics change.
    fn semantics_version(&self) -> u32 {
        1
    }

    fn device_profile(&self) -> DeviceProfile {
        DeviceProfile {
            disk: DiskBus::Virtio,
            net: NetModel::Virtio,
            video: Video::Vga,
            no_reboot: false,
        }
    }

    fn wait_ready(&self, ssh: &SshSession) -> Result<Readiness> {
        ready::wait(ssh)
    }

    fn shell_argv(&self, shell: Option<Shell>, script: &str) -> Result<Vec<String>> {
        Ok(match shell {
            None | Some(Shell::Pwsh) => ["pwsh", "-NoProfile", "-NonInteractive", "-Command"]
                .iter()
                .map(|part| (*part).to_owned())
                .chain([format!(
                    "{UTF8_OUTPUT}; & ([scriptblock]::Create({}))",
                    wrap::literal(script)
                )])
                .collect(),
            Some(Shell::Cmd) => {
                // A block scalar ends with a line end, which is not a second line.
                let script = script.trim_end_matches(['\r', '\n']);
                if script.contains(['\r', '\n']) {
                    return Err(Error::new(
                        ErrorKind::Recipe,
                        "`shell: cmd` runs one line: cmd.exe cannot be given a line break in a command line. Use \
                         `shell: pwsh`, or copy a `.cmd` file in and run it",
                    ));
                }
                ["cmd", "/d", "/c"]
                    .iter()
                    .map(|part| (*part).to_owned())
                    .chain([script.to_owned()])
                    .collect()
            }
            Some(Shell::Sh | Shell::Bash) => {
                return Err(Error::new(
                    ErrorKind::Recipe,
                    "the shells `sh` and `bash` do not exist in a windows guest; it has `pwsh` and `cmd`",
                ));
            }
        })
    }

    fn setting_command(&self, setting: &Setting) -> Result<Command> {
        match setting {
            Setting::Uac(UacMode::AutoApprove) => Ok(Command {
                argv: self.shell_argv(
                    Some(Shell::Pwsh),
                    &format!(
                        "Set-ItemProperty -Path '{UAC_POLICY}' -Name ConsentPromptBehaviorAdmin -Value 0 -Type DWord"
                    ),
                )?,
                run_as: RunAs::Elevated,
                cwd: None,
            }),
        }
    }

    fn file_name_problem(&self, name: &str) -> Option<String> {
        names::windows_problem(name)
    }

    fn extract_command(&self, destination: &str, archive: &str) -> Result<Command> {
        extract::extract_command(destination, archive)
    }

    fn copy_scratch_dir(&self) -> &'static str {
        extract::SCRATCH_DIR
    }
}
