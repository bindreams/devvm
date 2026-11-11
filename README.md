# devvm

Disposable virtual machines for tests: recipes, a layer cache, and QEMU.

## Install

Windows x86_64.

Prerequisites:

- Rust 1.89 or newer, the MSVC toolchain (`stable-x86_64-pc-windows-msvc`), with the Visual Studio Build Tools
  (C++ workload) it needs to link and to compile its C dependencies. No CMake or NASM is needed.
- The Windows Hypervisor Platform: in an elevated PowerShell,
  `Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform`, then restart.
- `git` and OpenSSH's `ssh` on `PATH`.

Install from `main`:

```sh
cargo install --git https://github.com/bindreams/devvm --locked devvm-cli
```

This puts `devvm.exe` in `~/.cargo/bin`. A cold build takes about one minute on a 32-core machine.
Run the same command with `--force` to upgrade.

Check the host:

```sh
devvm doctor
```

`doctor` prints one `pass` or `fail` line per check (accelerator, free space, `ssh`, `git`, QEMU) and exits 125 if any
failed. It downloads nothing: QEMU is downloaded on first use.

`devvm --help` lists the commands.
