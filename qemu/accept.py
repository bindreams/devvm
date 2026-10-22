#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Acceptance checks for the QEMU release zip: `accept.py <zip> <work-dir>`.

Extracts the zip into `<work-dir>/Anna Ж/qemu zip` (a space and a non-ASCII character in every path) and runs every
check with QEMU children on a reduced PATH (System32 and the Windows directory only). Prints one `ok:` or `FAIL:` line
per check and exits non-zero if any failed. Meant for a runner whose ANSI code page is not UTF-8: the first check
fails on 65001, since the rest would then prove nothing.
"""

import json
import os
import shutil
import subprocess
import sys
import winreg
import zipfile
from pathlib import Path, PureWindowsPath

HERE = Path(__file__).resolve().parent
PIN = dict(line.split("=", 1) for line in (HERE / "pin.env").read_text().splitlines() if "=" in line)
VERSION = PIN["QEMU_VERSION"]

CHECKS = []


def check(func):
    CHECKS.append(func)
    return func


class Zip:
    """The extracted zip and the environment QEMU children run in."""

    def __init__(self, archive: Path, work: Path):
        self.user_dir = work / "Anna Ж"
        self.root = self.user_dir / "qemu zip"
        if self.user_dir.exists():
            shutil.rmtree(self.user_dir)
        self.root.mkdir(parents=True)
        with zipfile.ZipFile(archive) as z:
            z.extractall(self.root)
        system_root = os.environ["SystemRoot"]
        self.env = {**os.environ, "PATH": f"{system_root}\\System32;{system_root}"}
        self.original_env = dict(os.environ)
        self.bin = self.root / "bin"
        self.share = self.root / "share"

    def run(self, argv, **kwargs) -> subprocess.CompletedProcess:
        """Run a QEMU tool; fail with its output if it exits non-zero."""
        done = subprocess.run([str(a) for a in argv], env=self.env, capture_output=True, **kwargs)
        stdout = done.stdout.decode("utf-8", errors="replace")
        stderr = done.stderr.decode("utf-8", errors="replace")
        if done.returncode != 0:
            raise AssertionError(f"{argv[0]} exited {done.returncode}\nstdout: {stdout}\nstderr: {stderr}")
        return subprocess.CompletedProcess(done.args, 0, stdout, stderr)


def registry_acp() -> str:
    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Nls\CodePage") as key:
        return winreg.QueryValueEx(key, "ACP")[0]


@check
def acp_is_not_utf8(z: Zip):
    acp = registry_acp()
    print(f"info: system ANSI code page (ACP) is {acp}")
    assert acp != "65001", "the ANSI code page is UTF-8, so the checks below would pass without the manifest"


@check
def version_and_accel(z: Zip):
    for system in ("qemu-system-x86_64", "qemu-system-aarch64"):
        out = z.run([z.bin / f"{system}.exe", "--version"]).stdout
        assert out.startswith(f"QEMU emulator version {VERSION}"), f"{system} --version printed: {out!r}"
    accels = z.run([z.bin / "qemu-system-x86_64.exe", "-accel", "help"]).stdout.split()
    assert "whpx" in accels, f"-accel help lists {accels}"


@check
def data_dir_is_the_zips_share(z: Zip):
    # No -L anywhere in these checks: QEMU must find the zip's share/ (ROMs, keymaps) relative to its own executable.
    for system in ("qemu-system-x86_64", "qemu-system-aarch64"):
        out = z.run([z.bin / f"{system}.exe", "-L", "help"]).stdout
        searched = [os.path.normcase(os.path.normpath(line)) for line in out.splitlines() if line]
        expected = os.path.normcase(os.path.normpath(z.root / "share"))
        assert expected in searched, f"{system} -L help lists {out!r}, not {expected}"


@check
def qemu_img_backing_chain_at_unicode_path(z: Zip):
    qemu_img = z.bin / "qemu-img.exe"
    base = z.user_dir / "base image.qcow2"
    overlay = z.user_dir / "overlay image.qcow2"
    z.run([qemu_img, "create", "-f", "qcow2", base, "1M"])
    z.run([qemu_img, "create", "-f", "qcow2", "-b", base, "-F", "qcow2", overlay])
    chain = json.loads(z.run([qemu_img, "info", "--backing-chain", "--output=json", overlay]).stdout)
    names = [PureWindowsPath(image["filename"]) for image in chain]
    assert names == [PureWindowsPath(overlay), PureWindowsPath(base)], f"backing chain is {names}"


class Qmp:
    """A synchronous QMP session over the pipes of a QEMU process."""

    def __init__(self, argv, env, stderr_path: Path):
        self.stderr_path = stderr_path
        self.stderr = open(stderr_path, "wb")
        self.proc = subprocess.Popen(
            [str(a) for a in argv], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, env=env
        )

    def stderr_text(self) -> str:
        return self.stderr_path.read_text(encoding="utf-8", errors="replace")  # QEMU writes UTF-8 (activeCodePage)

    def receive(self) -> dict:
        # Blocks until QEMU speaks or exits; EOF means it exited, usually because a path did not open.
        line = self.proc.stdout.readline()
        if not line:
            code = self.proc.wait()
            self.stderr.flush()
            raise AssertionError(f"QEMU exited {code} before replying; stderr: {self.stderr_text()}")
        return json.loads(line)

    def command(self, name: str) -> dict:
        self.proc.stdin.write((json.dumps({"execute": name}) + "\n").encode())
        self.proc.stdin.flush()
        while True:
            message = self.receive()
            if "return" in message or "error" in message:
                return message

    def close(self, quit_acknowledged: bool) -> int:
        """Reap QEMU. After an acknowledged `quit` it exits by itself; otherwise it would idle on its closed stdin."""
        if not quit_acknowledged:
            self.proc.kill()
        try:
            self.proc.stdin.close()
        except OSError:
            pass  # QEMU already exited; its exit code is checked by the caller
        self.proc.stdout.read()  # to EOF, so QEMU never writes (its SHUTDOWN event) into a closed pipe
        self.proc.stdout.close()
        code = self.proc.wait()
        self.stderr.close()
        return code


def firmware_and_drive_open(z: Zip, system: str, machine: list, code: str, vars_template: str):
    log = z.user_dir / f"serial {system}.log"
    vars_copy = z.user_dir / f"vars {system}.fd"
    shutil.copyfile(z.share / vars_template, vars_copy)
    overlay = z.user_dir / "overlay image.qcow2"
    if not overlay.exists():
        z.run([z.bin / "qemu-img.exe", "create", "-f", "qcow2", overlay, "1M"])
    argv = [
        z.bin / f"{system}.exe", *machine, "-accel", "tcg", "-m", "256", "-S",
        "-display", "none", "-monitor", "none", "-parallel", "none", "-qmp", "stdio",
        "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={z.share / code}",
        "-drive", f"if=pflash,format=raw,unit=1,file={vars_copy}",
        "-drive", f"if=none,id=disk0,format=qcow2,file={overlay}",
        "-device", "virtio-blk-pci,drive=disk0",
        "-nic", "user",
        "-serial", f"file:{log}",
    ]  # fmt: skip
    session = Qmp(argv, z.env, z.user_dir / f"stderr {system}.txt")
    quit_acknowledged = False
    try:
        greeting = session.receive()
        assert "QMP" in greeting, f"first message is not a QMP greeting: {greeting}"
        assert "return" in session.command("qmp_capabilities")
        status = session.command("query-status")["return"]
        assert status["running"] is False, f"query-status: {status}"
        assert "return" in session.command("quit")
        quit_acknowledged = True
    finally:
        code_ = session.close(quit_acknowledged)
    assert code_ == 0, f"{system} exited {code_}; stderr: {session.stderr_text()}"
    assert log.exists(), f"{log} was not created"


@check
def firmware_and_drive_open_x86_64(z: Zip):
    firmware_and_drive_open(
        z, "qemu-system-x86_64", ["-machine", "q35"], "edk2-x86_64-code.fd", "edk2-i386-vars.fd"
    )


@check
def firmware_and_drive_open_aarch64(z: Zip):
    firmware_and_drive_open(
        z, "qemu-system-aarch64", ["-machine", "virt", "-cpu", "max"], "edk2-aarch64-code.fd", "edk2-arm-vars.fd"
    )


@check
def dependencies_and_manifest(z: Zip):
    # Needs uv, so it runs with the original PATH; only QEMU children get the reduced one.
    uv = shutil.which("uv", path=z.original_env.get("PATH"))
    assert uv is not None, "uv is not on PATH"
    done = subprocess.run(
        [uv, "run", "--script", str(HERE / "pe_deps.py"), "check", str(z.root)],
        env=z.original_env, capture_output=True,
    )  # fmt: skip
    output = done.stdout.decode("utf-8", errors="replace") + done.stderr.decode("utf-8", errors="replace")
    print(output.rstrip())
    assert done.returncode == 0, "pe_deps.py check failed"


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    z = Zip(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
    failed = 0
    for func in CHECKS:
        try:
            func(z)
        except Exception as error:  # noqa: BLE001 - report every check, whatever it raised
            failed += 1
            print(f"FAIL: {func.__name__}: {error}")
        else:
            print(f"ok: {func.__name__}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
