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
failed, or prints an error and exits 125 if it cannot find devvm's directories. It downloads nothing: QEMU is downloaded on first use.

`devvm --help` lists the commands.
