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

## Usage

Each instance has a name: 1 to 63 of the ASCII letters, digits and `-`, starting and ending with a letter or digit,
not a Windows device name (`con`, `nul`, `com1`, ...), and lowercased (`--name Dev` makes `dev`). `up` builds a guest
from a recipe, starts it and prints its name; `ssh`, `cp`, `stop`, `start` and `rm` take that name.

Examples are for Git Bash. Git Bash rewrites arguments that look like paths (`/tmp/x`, `/c`), also after `--`: set
`MSYS_NO_PATHCONV=1` for such a command, and write its local paths as `C:/x` or relative ones.

### A Linux guest

The guest account of the embedded `ubuntu-24.04` is `user`, with passwordless `sudo`.

```sh
devvm up ubuntu-24.04 --name dev
devvm ssh dev
devvm ssh dev -- uname -a
devvm cp blob.bin dev:blob.bin
devvm cp dev:blob.bin blob.back
devvm ls
devvm stop dev
devvm start dev
devvm rm dev
```

- A command that needs a guest (`up`, `run` or `build`) downloads QEMU and the guest image the first time, with
  progress on stderr. For `up` and `run`, `--cpus` and `--memory` size the guest and `--network none` cuts it off from
  the internet; `--arch arm64` (also for `build`) runs under software emulation, slowly.
- `ssh` with no command opens a login shell. `devvm ssh dev -- <command>` runs one command and exits with its status.
- The words after `--` are one argv, not a shell string, and nothing in them is expanded by the guest: `-- 'a; b'`
  looks for a program named `a; b` and exits 127. For a pipeline, `~` or `$HOME`, run a shell:
  `devvm ssh dev -- bash -c 'cd ~/proj && ls'`. `devvm run` takes its command the same way.
- `cp` takes `INSTANCE:PATH` on one side and a local path on the other. A relative guest path starts in the user's
  home. `devvm cp --help` lists the names it cannot carry.
- `ls` lists the instances created from the current directory; `ls --all` lists every one.
- `stop` keeps the disk; `start` boots it again. The guest's `/tmp` is empty after a restart: keep files under `~`.

### A project in a recipe

A recipe file ends in `.devvm.yaml`. Put this one in a git repository: it starts from the embedded recipe and copies
the files git tracks there into the guest (an untracked or ignored file stays behind).

```yaml
from: ubuntu-24.04
steps:
  - copy: { from: git-tracked, to: ~/proj }
```

```sh
devvm run proj.devvm.yaml -- bash -c 'cd ~/proj && ls -R'
```

`run` boots a throwaway guest from the recipe, runs the command, destroys the guest and exits with the command's
status. `devvm up proj.devvm.yaml --name dev` keeps the guest instead. `devvm build proj.devvm.yaml` builds the image
and starts nothing.

Each step's result is cached as a layer: a rerun rebuilds from the first step that changed.

What a recipe file takes (an unknown key is an error):

- `from:` (required) is one of:
  - `ubuntu-24.04`, the embedded recipe;
  - another recipe file, `../base.devvm.yaml`, relative to this file;
  - `{ iso, edition, product-key }`, a Windows ISO (see [Windows ISOs](#windows-isos));
  - `{ url, sha256 }`, a Linux cloud image with cloud-init (qcow2 or another format `qemu-img` converts); `amd64:` and
    `arm64:` each take one.
- `name:` names the recipe; the default is the file name without `.devvm.yaml`.
- `resources:` takes `cpus`, `memory` and `disk`, as `8GiB` or a byte count.
- `steps:` run in order. A step is exactly one of:
  - `run: <script>`, with optional `shell: bash | sh | pwsh | cmd`, `elevated: true` and `cwd: <guest path>`. The
    default shell is `bash` on Linux and `pwsh` on Windows. A Linux guest refuses `cmd`, and has `pwsh` only if an
    earlier step installs it (`ubuntu-24.04` has none).
  - `copy: { from: git-tracked, to: <guest path>, read-only: true }`. It copies the files git tracks in the repository
    that contains the recipe, as they are on disk; `read-only` is optional. Relative `to` paths and `~` start in the
    user's home.
  - `set: { uac: auto-approve }`, on Windows only.

### A Windows guest

`win/windows.devvm.yaml`, from an installer ISO you downloaded ([Windows ISOs](#windows-isos) has the rules for
`edition` and `product-key`):

```yaml
from:
  iso: C:/isos/windows.iso
  edition: Windows 11 IoT Enterprise LTSC 2024
  product-key: XXXXX-XXXXX-XXXXX-XXXXX-XXXXX
```

```sh
devvm up win/windows.devvm.yaml --name win
devvm ssh win -- whoami
devvm ssh win
devvm cp blob.bin win:blob.bin
devvm cp win:blob.bin blob.back
devvm stop win
devvm start win
devvm rm win
```

A project's recipe can start from it and add steps. `proj/proj-win.devvm.yaml`, where `proj/` is the root of the
project's git repository and `win/` sits beside it, outside the repository, so the recipe holding the key is neither
committed nor copied in by `copy` (Windows Setup still installs the key in the guest):

```yaml
from: ../win/windows.devvm.yaml
steps:
  - copy: { from: git-tracked, to: ~/proj }
```

```sh
devvm run proj/proj-win.devvm.yaml -- pwsh -c 'Get-ChildItem -Recurse -Name ~/proj'
```

- The first command that needs the Windows base builds it from the ISO, which takes minutes. The build downloads the
  virtio drivers from fedorapeople.org (about 0.9 GB) and OpenSSH, PowerShell 7 and ssh-broker from github.com. Later
  commands reuse the base. An evaluation edition's base is rebuilt from the ISO 10 days before its evaluation ends; a
  base built with a product key does not expire.
- `ssh win` opens PowerShell 7; the home (`~`, and where relative guest paths start) is the user's profile.
- A `cmd` builtin such as `dir` is not a program: `devvm ssh win -- dir` exits 127 (it is also a PowerShell alias: run
  it through `pwsh -c`). For `cmd`'s, use `MSYS_NO_PATHCONV=1 devvm ssh win -- cmd /c dir`.
- [Windows guests](#windows-guests) has how programs, shells, paths and `copy` work in a Windows guest.

## Windows ISOs

A recipe starts a Windows guest from an installer ISO you downloaded:

```yaml
from: { iso: ../isos/windows.iso, edition: Windows 11 Enterprise Evaluation }
```

- `edition` is the image's name in the ISO's `install.wim` (or `install.esd`). An ISO with one image does not need it;
  with several, the error lists their names.
- A non-evaluation edition needs `product-key: XXXXX-XXXXX-XXXXX-XXXXX-XXXXX` in `from:`, and an evaluation edition takes
  none; the build is refused, before anything boots, when a non-evaluation edition has none or an evaluation edition has
  one. devvm checks the key's shape only, not which edition it is for.
- Microsoft publishes generic KMS client setup keys for its volume editions: see
  [Key Management Services (KMS) client activation and product keys](https://learn.microsoft.com/en-us/windows-server/get-started/kms-client-activation-keys).
- Windows installs unactivated with one of those keys; activate it however you are licensed.
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
The base has no Windows Recovery Environment.

- **Programs.** `argv[0]` is looked up as a program on the guest's `PATH`, never as a PowerShell cmdlet or alias. A name
  that is not found exits 127, and when it is a PowerShell command the message says to run it through `pwsh -c`. A
  program's output reaches you byte for byte, and a status the guest ends with (`0xC0000005`, say) is passed on in all
  32 bits on a Windows host; a Unix host keeps the low byte.
- **Shells.** A `run` step uses `pwsh` (PowerShell 7) unless it says `shell: cmd`. `sh` and `bash` do not exist in a
  Windows guest. A `cmd` script is one line, and a line break in it is refused before the build starts: use
  `shell: pwsh`, or `copy` a `.cmd` file in and run it.
- **`copy`.** A `copy` step extracts its archive with PowerShell. A tracked symlink ends the step with an error that
  names it: the guest's user cannot create one. A file that exists is overwritten, whatever its ReadOnly, Hidden or
  System attributes. A successful overwrite clears Hidden and System; ReadOnly follows the archive's mode (set when the
  entry has no owner-write bit). A failed write restores the file's old attributes. `read-only: true` sets the ReadOnly
  attribute on the copied files; directories stay writable.
- **Paths.** Write guest paths with `/`: `C:/x/y`, `~/x`. `~` is the user's profile; `~\x` and `C:\x` are refused with the
  spelling that works.
- **`elevated: true`, `--elevated`** run at High integrity with no prompt, through a scheduled task. Each costs 2 to 6
  seconds (5 to 7 seconds for the first in a boot). Any process of the user at Medium integrity can start that task, so
  the guest has no separation between the user and the administrator, as a Linux guest has none with its passwordless
  `sudo`.
- **`set: { uac: auto-approve }`** makes an administrator's elevation prompt-free in the guest, with no reboot.
- A command is refused when the command line that carries its script is over 32,766 UTF-16 units (the script is quoted
  as one argument, and the line is about 46 units longer than it), and the error names both lengths. Each argument is in
  the script twice, next to its fixed text, so the arguments get about 14,940 characters; the text of `cmd /c` is there
  a third time (about 9,920), and less fits when the command is elevated (about 11,440, and 7,580 for `cmd`). These are
  the figures at the time of writing; `longest_fit` in `crates/devvm/src/guest/windows/wrap_tests.rs` measures them.

Limits:

- After the guest's agent stops, `devvm ssh` and `devvm cp` run in session 0 at High until the instance is restarted;
  `devvm run` exits 126 "the command did not reach the user's session". A program that returns 126 looks the same;
  the message on stderr tells them apart.
- Piping large input through system `ssh` (interactive `devvm ssh <name>`, or your own `ssh` with devvm's generated
  config), or into a .NET program such as Windows PowerShell 5.1, can hang that session; `devvm stop` and `devvm start`
  recover it. `devvm run` and `devvm ssh <name> -- <command>` send no stdin.
- Interactive `devvm ssh <name>` and `ssh -t` end with status 0 on a Windows guest; commands keep their status.
- On a Windows guest, a session that asks for a terminal (`ssh -t`/`-tt`) while its input is not a terminal (`</dev/null`,
  a pipe that ends, or a library that sends end-of-input) may end with status 0 without running the command. Drop `-t`,
  or keep the input open.
- devvm does not end a guest command whose client went away (a killed `devvm`, a closed reader). The command finds out
  when it next writes, as a command run by ssh without a terminal does. A command that writes nothing keeps running
  until it ends or the instance stops (`devvm stop`).
