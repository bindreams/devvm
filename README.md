# devvm

Disposable virtual machines for tests: recipes, a layer cache, and QEMU.

## Install

Windows x86_64.

Prerequisites:

- Rust 1.89 or newer with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`) as the default (QEMU is published for
  that target only), and the Visual Studio Build Tools (C++ workload) it needs to link and to compile its C
  dependencies. No CMake or NASM is needed.
- Hardware virtualization turned on in the firmware (or nested virtualization, if the PC is itself a VM).
- The Windows Hypervisor Platform: in an elevated PowerShell,
  `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform`, then restart.
- `git` and OpenSSH's `ssh` on `PATH`.

Install from `main`:

```sh
cargo install --git https://github.com/bindreams/devvm --locked devvm-cli
```

This puts `devvm.exe` in `~/.cargo/bin`. A cold build takes about one minute on a 32-core machine.
Running the same command again installs the newer `main`.

Check the host:

```sh
devvm doctor
```

`doctor` prints one `pass` or `fail` line per check (accelerator, free space, `ssh`, `git`, QEMU) and exits 125 if any
failed, or prints an error and exits 125 if it cannot find devvm's directories. It downloads nothing: QEMU is
downloaded on first use.

`devvm --help` lists the commands.

## Windows ISOs

A recipe starts a Windows guest from an installer ISO you downloaded:

```yaml
from: { iso: ../isos/windows.iso, edition: Windows 11 Enterprise Evaluation }
```

- `edition` is the image's name in the ISO's `install.wim` (or `install.esd`). An ISO with one image does not need it;
  with several, the error lists their names.
- x64 images only. The install language is the image's default language.
- Keep the ISO at the recipe's path. devvm never copies it, and a rebuild before the base expires reads it again.
  The layer is keyed by the ISO's content, so moving or renaming it does not rebuild.
- On Windows, a build holds the ISO open while it makes the base layer: other programs can read it, but cannot change,
  move or delete it. An ISO that a program has open for writing is refused, naming the ISO.
- `devvm doctor` hashes every ISO a build has keyed again and fails on one that changed. One that is not found is listed
  as information: a moved ISO shows up so until a build names it at its new path.

## Windows guests

The guest account is `user`, a member of Administrators with UAC on, logged in automatically. Commands run as that
user, not elevated, through ssh-broker in the user's own session, so they see its profile and registry.

- **Programs.** `argv[0]` is looked up as a program on the guest's `PATH`, never as a PowerShell cmdlet or alias. A name
  that is not found exits 127. A program's output reaches you byte for byte, and a status the guest ends with
  (`0xC0000005`, say) is passed on in all 32 bits on a Windows host; a Unix host keeps the low byte.
- **Shells.** A `run` step uses `pwsh` (PowerShell 7) unless it says `shell: cmd`. `sh` and `bash` do not exist in a
  Windows guest.
- **Paths.** Write guest paths with `/`: `C:/x/y`, `~/x`. `~` is the user's profile; `~\x` and `C:\x` are refused with the
  spelling that works.
- **`elevated: true`, `--elevated`** run at High integrity with no prompt, through a scheduled task. Each costs 2 to 6
  seconds (5 to 7 seconds for the first in a boot). Any process of the user at Medium integrity can start that task, so
  the guest has no separation between the user and the administrator, as a Linux guest has none with its passwordless
  `sudo`.
- **`set: { uac: auto-approve }`** makes an administrator's elevation prompt-free in the guest, with no reboot.
- A command line can be at most 32,767 characters on the guest. A command whose script is longer once quoted is refused,
  naming both lengths.

Limits:

- After the guest's agent stops, `devvm ssh` and `devvm cp` run in session 0 at High until the instance is restarted;
  `devvm run` exits 126 "the command did not reach the user's session". A program that returns 126 looks the same;
  the message on stderr tells them apart.
- Piping large input through `devvm ssh` (system `ssh`), or into a .NET program such as Windows PowerShell 5.1, can hang
  that session; `devvm stop` and `devvm start` recover it. `devvm run` sends no stdin.
- Interactive `devvm ssh <name>` and `ssh -t` end with status 0 on a Windows guest; commands keep their status.
