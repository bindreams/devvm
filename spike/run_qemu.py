# /// script
# requires-python = ">=3.11"
# dependencies = ["pillow"]
# ///
"""Spike only (never merged): runs a Windows guest under QEMU/WHPX the way devvm's base build would (no_reboot, then
relaunch) and records what it does.

mode `install`: launch 1 has the install media; a launch that ends without `--done` in its serial log is a Setup
                reboot and is relaunched without the install media, on the same disk and vars.fd. A `DEVVM-BASE-FAILED=`
                line fails the run once QEMU has exited. `--single` makes the first launch the only one.
mode `stage`:   one launch of an existing disk. After `--done` (a marker line) shows up in the serial log, the guest is
                asked to power down (QMP `system_powerdown`, measure M8c) and QEMU exiting is the end.
                With `--driver CMD...` (the rest of the command line) the host runs CMD beside the guest instead,
                writing its output to `driver.log`; `--done` is ignored, the guest is asked to power down when CMD
                ends, and the run is ok when CMD exited 0. `--ssh-port N` forwards 127.0.0.1:N to the guest's port 22.

Every launch writes `launch-N/` under the output directory: serial.log, qemu-args.txt, QEMU's stdout and stderr, the QMP
events with host timestamps, a PNG whenever the screen changed (at most one a minute) and `last.png`. The host clock of
every serial line goes to the console and to `timeline.txt`. The run has no deadline of its own; the workflow's job
timeout is the failure bound.

Usage: run_qemu.py <install|stage> --qemu-dir D --out D --disk F --vars F [--install-iso F] [--install-format raw|qcow2]
                   [--virtio-iso F] [--usb-disk F [--usb-writable]] [--done TEXT] [--single] [--cpu CPU] [--smp N] [--mem MiB]
                   [--ssh-port N] [--driver CMD...]
"""

import argparse
import hashlib
import json
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

from PIL import Image

QMP_PORT = 4455
POLL_SECONDS = 15  # screenshot and serial-log cadence: how often the run looks, never a condition
SCREEN_EVERY = 60
QMP_REPLY_BOUND = 120  # seconds a QMP command may go unanswered before the run says so and goes on
FAILED = "DEVVM-BASE-FAILED="


class Qmp:
    def __init__(self, events: Path, qemu: subprocess.Popen) -> None:
        self.events = events.open("w", encoding="utf-8")
        self.replies: list[dict] = []
        self.cond = threading.Condition()
        self.closed = False
        self.sock = None
        while self.sock is None:  # refusal is the real "not yet"; QEMU exiting ends the wait
            try:
                self.sock = socket.create_connection(("127.0.0.1", QMP_PORT))
            except ConnectionRefusedError:
                if qemu.poll() is not None:
                    self.closed = True
                    return
                time.sleep(0.2)
        self.rfile = self.sock.makefile("r", encoding="utf-8", newline="\n")
        self.wfile = self.sock.makefile("w", encoding="utf-8", newline="\n")
        threading.Thread(target=self.read, daemon=True).start()
        self.command("qmp_capabilities")

    def read(self) -> None:
        try:
            for line in self.rfile:
                message = json.loads(line)
                if "event" in message or "QMP" in message:
                    self.events.write(json.dumps({"host_time": time.time(), **message}) + "\n")
                    self.events.flush()
                else:
                    with self.cond:
                        self.replies.append(message)
                        self.cond.notify_all()
        except OSError:  # QEMU has gone
            pass
        with self.cond:
            self.closed = True
            self.cond.notify_all()

    def command(self, name: str, **arguments) -> dict | None:
        if self.sock is None:
            return None
        try:
            self.wfile.write(json.dumps({"execute": name, "arguments": arguments}) + "\n")
            self.wfile.flush()
        except OSError:
            return None
        with self.cond:
            # The reply is an external event that may never come (a hung QEMU); the bound is surfaced by the caller.
            self.cond.wait_for(lambda: self.replies or self.closed, timeout=QMP_REPLY_BOUND)
            return self.replies.pop(0) if self.replies else None


def find(root: Path, name: str) -> Path:
    return next(root.rglob(name))


def qemu_args(a: argparse.Namespace, out: Path, with_install_media: bool) -> list[str]:
    qemu_dir = Path(a.qemu_dir)
    share = find(qemu_dir, "edk2-x86_64-code.fd").parent
    args = [
        str(find(qemu_dir, "qemu-system-x86_64.exe")),
        "-accel", "whpx", "-machine", "q35", "-cpu", a.cpu, "-smp", str(a.smp), "-m", str(a.mem),
        "-display", "none", "-nodefaults", "-no-user-config", "-no-reboot", "-rtc", "base=utc",
        "-drive", f"if=pflash,format=raw,readonly=on,unit=0,file={share / 'edk2-x86_64-code.fd'}",
        "-drive", f"if=pflash,format=raw,unit=1,file={a.vars}",
        "-device", "VGA",
        "-device", "ich9-ahci,id=ahci",
        "-drive", f"if=none,id=d0,file={a.disk},format=qcow2",
        "-device", "virtio-blk-pci,drive=d0,bootindex=2",
        "-netdev", "user,id=n0" + (f",hostfwd=tcp:127.0.0.1:{a.ssh_port}-:22" if a.ssh_port else ""),
        "-device", "virtio-net-pci,netdev=n0",
        "-device", "qemu-xhci,id=xhci",
        "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server=on,wait=off",
        "-serial", f"file:{out / 'serial.log'}",
    ]  # fmt: skip
    if with_install_media:
        args += [
            "-drive", f"if=none,id=cd0,file={a.install_iso},format={a.install_format},media=cdrom,readonly=on",
            "-device", "ide-cd,drive=cd0,bus=ahci.0,bootindex=1",
        ]  # fmt: skip
    if a.virtio_iso:
        args += [
            "-drive", f"if=none,id=cd1,file={a.virtio_iso},format=raw,media=cdrom,readonly=on",
            "-device", "ide-cd,drive=cd1,bus=ahci.1",
        ]  # fmt: skip
    if a.usb_disk:
        args += [
            "-drive", f"if=none,id=usb0,file={a.usb_disk},format=raw,readonly={'off' if a.usb_writable else 'on'}",
            "-device", "usb-storage,drive=usb0,removable=on,bus=xhci.0",
        ]  # fmt: skip
    return args


class Run:
    def __init__(self, a: argparse.Namespace) -> None:
        self.a = a
        self.out = Path(a.out)
        self.out.mkdir(parents=True, exist_ok=True)
        self.started = time.time()
        self.timeline = (self.out / "timeline.txt").open("w", encoding="utf-8")
        self.summary: dict = {"launches": [], "markers": []}
        self.driver_code: int | None = None

    def save(self) -> None:
        """Rewrites summary.json, so a run killed by a step timeout still leaves its markers and launch times."""
        self.summary["seconds"] = round(time.time() - self.started, 1)
        (self.out / "summary.json").write_text(json.dumps(self.summary, indent=2) + "\n", encoding="utf-8")

    def say(self, text: str) -> None:
        line = f"[{time.time() - self.started:8.1f}s] {text}"
        print(line, flush=True)
        self.timeline.write(line + "\n")
        self.timeline.flush()

    def launch(self, n: int, with_install_media: bool) -> tuple[int, str]:
        """One QEMU process, to its exit. Returns the exit code and the whole serial log."""
        out = self.out / f"launch-{n}"
        out.mkdir(exist_ok=True)
        args = qemu_args(self.a, out, with_install_media)
        (out / "qemu-args.txt").write_text("\n".join(args) + "\n", encoding="utf-8")
        self.say(f"launch {n}: starting QEMU ({'with' if with_install_media else 'without'} install media)")
        began = time.time()
        serial = out / "serial.log"
        seen = 0
        text = ""
        last_hash = None
        last_saved = 0.0
        shots = 0
        powered_down = False
        driver = None
        with (out / "qemu-stdout.log").open("w") as so, (out / "qemu-stderr.log").open("w") as se:
            qemu = subprocess.Popen(args, stdout=so, stderr=se)
            if self.a.driver:
                driver_log = (out / "driver.log").open("wb")
                driver = subprocess.Popen(self.a.driver, stdout=driver_log, stderr=subprocess.STDOUT)
            qmp = Qmp(out / "qmp-events.jsonl", qemu)
            while qemu.poll() is None:
                ppm = out / "frame.ppm"
                reply = qmp.command("screendump", filename=str(ppm))
                if reply is not None and "return" in reply and ppm.exists():
                    data = ppm.read_bytes()
                    digest = hashlib.sha256(data).hexdigest()
                    image = Image.open(ppm)
                    image.save(out / "last.png")
                    if digest != last_hash and time.time() - last_saved >= SCREEN_EVERY:
                        shots += 1
                        image.save(out / f"screen-{shots:04d}.png")
                        last_saved = time.time()
                        last_hash = digest  # only a saved frame counts, so a change inside the minute is saved after it
                    image.close()
                elif qemu.poll() is None:
                    self.say("screendump: QMP gave no usable reply")
                text, seen = self.read_serial(serial, text, seen)
                self.summary["current"] = {
                    "launch": n, "serial_bytes": seen, "screenshots": shots, "seconds_in_launch": round(time.time() - began, 1),
                }  # fmt: skip
                self.save()
                if driver is not None and driver.poll() is not None and not powered_down:
                    self.driver_code = driver.returncode
                    self.say(f"driver ended with {driver.returncode}: asking the guest to power down")
                    qmp.command("system_powerdown")
                    powered_down = True
                elif driver is None and self.a.mode == "stage" and not powered_down and self.a.done and self.a.done in text:
                    self.say("stage marker seen: asking the guest to power down (system_powerdown)")
                    qmp.command("system_powerdown")
                    powered_down = True
                try:
                    qemu.wait(timeout=POLL_SECONDS)  # pacing between looks, not a condition
                except subprocess.TimeoutExpired:
                    pass
            code = qemu.wait()
        if driver is not None and driver.poll() is None:
            self.say("QEMU exited before the driver ended: stopping the driver")
            driver.terminate()
            driver.wait()
        if driver is not None and self.driver_code is None:
            self.driver_code = driver.returncode
        text, seen = self.read_serial(serial, text, seen)
        self.say(f"launch {n}: QEMU exited with {code} after {time.time() - began:.0f}s")
        self.summary["launches"].append(
            {"n": n, "install_media": with_install_media, "exit_code": code, "seconds": round(time.time() - began, 1),
             "screenshots": shots}
        )  # fmt: skip
        return code, text

    def read_serial(self, path: Path, text: str, seen: int) -> tuple[str, int]:
        try:
            full = path.read_bytes().decode("utf-8", errors="replace")
        except OSError:  # QEMU holds it open; the next look or the exit gets it
            return text, seen
        for line in full[seen:].splitlines():
            if line.strip():
                self.say(f"serial: {line[:300]}")
                if line.startswith("DEVVM-"):
                    self.summary["markers"].append({"at": round(time.time() - self.started, 1), "line": line[:300]})
        return full, len(full)

    def finish(self, ok: bool) -> None:
        self.summary["ok"] = ok
        self.summary.pop("current", None)
        self.save()
        self.say(f"summary: ok={ok}")
        sys.exit(0 if ok else 1)


def main() -> None:
    # The runner's console is cp1252; guest text (mojibake, German output) must never end the run.
    sys.stdout.reconfigure(encoding="utf-8", errors="backslashreplace")
    sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
    p = argparse.ArgumentParser()
    p.add_argument("mode", choices=["install", "stage"])
    p.add_argument("--qemu-dir", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--disk", required=True)
    p.add_argument("--vars", required=True)
    p.add_argument("--install-iso")
    p.add_argument("--install-format", default="raw")
    p.add_argument("--virtio-iso")
    p.add_argument("--usb-disk")
    p.add_argument("--usb-writable", action="store_true")
    p.add_argument("--done", default="")
    p.add_argument("--single", action="store_true")
    p.add_argument("--cpu", default="max")
    p.add_argument("--smp", type=int, default=2)
    p.add_argument("--mem", type=int, default=4096)
    p.add_argument("--ssh-port", type=int)
    p.add_argument("--driver", nargs=argparse.REMAINDER, default=[])
    a = p.parse_args()

    run = Run(a)
    run.say("qemu: " + subprocess.run([str(find(Path(a.qemu_dir), "qemu-system-x86_64.exe")), "--version"],
                                      capture_output=True, text=True, check=True).stdout.splitlines()[0])  # fmt: skip
    n = 1
    while True:
        code, text = run.launch(n, with_install_media=(a.mode == "install" and n == 1))
        if FAILED in text:
            run.say("the guest reported a failure: " + text[text.index(FAILED):][:2000])
            run.finish(False)
        if code != 0:
            run.say(f"QEMU failed with {code}; see launch-{n}/qemu-stderr.log")
            run.finish(False)
        if a.mode == "stage" and a.driver:
            run.finish(run.driver_code == 0)
        if a.mode == "stage":
            run.finish(a.done in text)
        if a.single:
            run.finish(True)
        if a.done and a.done in text:
            run.finish(True)
        n += 1  # no marker and a clean exit: Setup rebooted; relaunch without the install media


main()
