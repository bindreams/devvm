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

import itertools
import json
import os
import shutil
import subprocess
import sys
import threading
import winreg
import zipfile
from pathlib import Path, PureWindowsPath

HERE = Path(__file__).resolve().parent
PIN = dict(line.split("=", 1) for line in (HERE / "pin.env").read_text().splitlines() if "=" in line)
VERSION = PIN["QEMU_VERSION"]

CHECKS = []

# Failure bounds on external processes that might hang; expiry fails the check, it never proves state.
RUN_BOUND = 120  # seconds for one QEMU tool invocation
QEMU_BOUND = 180  # seconds for one QEMU system emulator session
ERROR_PIPE_BUSY = 231


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
        try:
            done = subprocess.run([str(a) for a in argv], env=self.env, capture_output=True, timeout=RUN_BOUND, **kwargs)
        except subprocess.TimeoutExpired:
            raise AssertionError(f"{argv[0]} did not finish within {RUN_BOUND}s and was killed") from None
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
    """A synchronous QMP session over a named pipe, the transport devvm itself uses.

    QEMU's `-chardev pipe` creates the pipe in the Windows pipe namespace and blocks its own start-up in
    ConnectNamedPipe until a client opens it, so `__init__` connects as soon as the pipe exists. QEMU is an external
    process that may hang, so a watchdog kills it after `QEMU_BOUND` seconds: every blocked read then ends, and the
    check fails instead of the job.
    """

    ids = itertools.count()

    def __init__(self, argv, env, log_path: Path):
        self.log_path = log_path
        self.log = open(log_path, "wb")
        name = f"devvm-accept-{os.getpid()}-{next(self.ids)}"
        self.pipe_path = rf"\\.\pipe\{name}"
        self.expired = False
        self.proc = subprocess.Popen(
            [*(str(a) for a in argv), "-chardev", f"pipe,id=qmp,path={name}", "-mon", "chardev=qmp,mode=control"],
            stdin=subprocess.DEVNULL, stdout=self.log, stderr=subprocess.STDOUT, env=env,
        )  # fmt: skip
        self.watchdog = threading.Timer(QEMU_BOUND, self.expire)
        self.watchdog.daemon = True
        self.watchdog.start()
        try:
            self.pipe = self.connect()
        except BaseException:
            self.close(quit_acknowledged=False)
            raise

    def expire(self):
        self.expired = True
        self.proc.kill()

    def log_text(self) -> str:
        if not self.log.closed:
            self.log.flush()
        return self.log_path.read_text(encoding="utf-8", errors="replace")  # QEMU writes UTF-8 (activeCodePage)

    def explain(self, what: str) -> AssertionError:
        if self.expired:
            return AssertionError(f"QEMU did not {what} within {QEMU_BOUND}s and was killed; output: {self.log_text()}")
        return AssertionError(f"QEMU exited {self.proc.wait()} before it could {what}; output: {self.log_text()}")

    def connect(self):
        # Repeats while QEMU is alive and has not created the pipe yet; the watchdog bounds a QEMU that never does.
        while True:
            try:
                return open(self.pipe_path, "r+b", buffering=0)
            except OSError as error:
                if not (isinstance(error, FileNotFoundError) or error.winerror == ERROR_PIPE_BUSY):
                    raise
            try:
                self.proc.wait(timeout=0.02)
            except subprocess.TimeoutExpired:
                continue
            raise self.explain("open the QMP pipe")

    def receive(self) -> dict:
        try:
            line = self.pipe.readline()  # blocks until QEMU speaks, exits or is killed
        except OSError:
            line = b""  # a broken pipe is EOF
        if not line:
            raise self.explain("answer on the QMP pipe")
        return json.loads(line)

    def command(self, name: str) -> dict:
        data = (json.dumps({"execute": name}) + "\n").encode()
        try:
            while data:
                data = data[self.pipe.write(data) :]
        except OSError:
            raise self.explain(f"accept {name}") from None
        while True:
            message = self.receive()
            if "return" in message or "error" in message:
                return message

    def close(self, quit_acknowledged: bool) -> int:
        """Reap QEMU. After an acknowledged `quit` it exits by itself (or the watchdog kills it); otherwise kill it."""
        if not quit_acknowledged:
            self.proc.kill()
        try:
            if getattr(self, "pipe", None) is not None:
                try:
                    self.pipe.read()  # to EOF: QEMU has closed its end, so it is done writing
                except OSError:
                    pass  # broken pipe is EOF
                self.pipe.close()
            code = self.proc.wait()
        finally:
            self.watchdog.cancel()
            self.log.close()
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
        "-display", "none", "-monitor", "none", "-parallel", "none",
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
    assert code_ == 0, f"{system} exited {code_}; output: {session.log_text()}"
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
    try:
        done = subprocess.run(
            [uv, "run", "--script", str(HERE / "pe_deps.py"), "check", str(z.root)],
            env=z.original_env, capture_output=True, timeout=RUN_BOUND,
        )  # fmt: skip
    except subprocess.TimeoutExpired:
        raise AssertionError(f"pe_deps.py check did not finish within {RUN_BOUND}s and was killed") from None
    output = done.stdout.decode("utf-8", errors="replace") + done.stderr.decode("utf-8", errors="replace")
    print(output.rstrip())
    assert done.returncode == 0, "pe_deps.py check failed"


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    # Line-buffered: stdout is a pipe in CI, and a check that hangs must still have printed its name.
    sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace", line_buffering=True)
    z = Zip(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
    failed = 0
    for func in CHECKS:
        print(f"run: {func.__name__}")
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
