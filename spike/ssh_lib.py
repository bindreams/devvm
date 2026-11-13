"""Spike only (never merged): what the host side of every SSH-driven stage shares. `init` once, then `say`, `ssh`,
`probe` and `probe_until`. The log is `<out>/<log name>`; the probe is `probe.ps1` on the guest (`key=value` lines).
"""

import os
import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

SSH = r"C:\Windows\System32\OpenSSH\ssh.exe"
PROBE = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\probe.ps1"
KILL = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\killagent.ps1"
START_TASK = 'powershell.exe -NoProfile -Command "Start-ScheduledTask -TaskName {}"'
# A failure of these kinds is the answer, not "not yet".
FATAL = ("REMOTE HOST IDENTIFICATION HAS CHANGED", "Host key verification failed", "Permission denied")

out: Path
keys: Path
port: str
_log = None
_begin = 0.0


def init(out_dir: str, key_dir: str, ssh_port: str, log_name: str) -> None:
    global out, keys, port, _log, _begin
    out = Path(out_dir)
    keys = Path(key_dir)
    port = ssh_port
    out.mkdir(parents=True, exist_ok=True)
    _log = (out / log_name).open("a", encoding="utf-8", buffering=1)
    _begin = time.time()


def say(text: str) -> None:
    _log.write(f"[{time.time() - _begin:8.1f}s] {text}\n")


def ssh(command: str, known_hosts: str = "known_hosts", stdin: bytes | None = None, tty: bool = False,
        bound: float | None = None) -> tuple[int, bytes, bytes]:
    """`tty` forces a pty (`-tt`). `bound` ends a command that may never end (the caller's failure bound for an external
    event); `subprocess.TimeoutExpired` then names it."""
    args = [
        SSH, "-p", port, "-i", (keys / "client").as_posix(), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=yes", "-o", f"UserKnownHostsFile={(keys / known_hosts).as_posix()}", "-o", "ConnectTimeout=10",
        *(["-tt"] if tty else []), "user@127.0.0.1", command,
    ]  # fmt: skip
    done = subprocess.run(args, input=stdin if stdin is not None else b"", capture_output=True, timeout=bound)
    return done.returncode, done.stdout, done.stderr


def facts(stdout: bytes) -> dict[str, str]:
    found = {}
    for line in stdout.decode("utf-8", errors="replace").splitlines():
        key, sep, value = line.partition("=")
        if sep:
            found[key.strip()] = value.strip()
    return found


def probe(label: str) -> dict[str, str] | None:
    """One probe through the shim. None when SSH did not answer; a fatal SSH error ends the run."""
    code, stdout, stderr = ssh(PROBE)
    error = stderr.decode("utf-8", errors="replace").strip()
    for fatal in FATAL:
        if fatal in error:
            say(f"{label}: FATAL ssh error: {error}")
            sys.exit(1)
    if code != 0 and not stdout.strip():
        say(f"{label}: no answer (exit {code}): {error[:200]}")
        return None
    seen = facts(stdout)
    say(f"{label}: exit {code} session={seen.get('session')} integrity={seen.get('integrity')} "
        f"elevated={seen.get('elevated')} hkcu={seen.get('hkcu')} dpapi={seen.get('dpapi')} boot={seen.get('boot')} "
        f"now={seen.get('now')} sshd-created={seen.get('sshd-created')} explorer-created={seen.get('explorer-created')} "
        f"agents={seen.get('agents')} parent={seen.get('parent')}")  # fmt: skip
    return seen


def probe_until(label: str, accept: Callable[[dict[str, str]], bool]) -> dict[str, str]:
    """Probes until `accept` holds for an answer. The wait re-checks the condition; no cap."""
    while True:
        seen = probe(label)
        if seen is not None and accept(seen):
            return seen
        time.sleep(1)  # pacing between probes


def in_session(session: str, integrity: str) -> Callable[[dict[str, str]], bool]:
    return lambda seen: (seen.get("session"), seen.get("integrity")) == (session, integrity)


def switch_agent(label: str, task: str, accept: Callable[[dict[str, str]], bool]) -> None:
    """Ends the serving agent (the connection drops), starts `task` from the plain session sshd makes, waits for it."""
    code, _, _ = ssh(KILL)
    say(f"{label}: the serving agent was killed (exit {code})")
    code, _, error = ssh(START_TASK.format(task))
    say(f"{label}: Start-ScheduledTask {task} exit {code} {error.decode('utf-8', errors='replace').strip()[:200]}")
    began = time.time()
    seen = probe_until(label, accept)
    say(f"{label}: answered in the expected session after {time.time() - began:.1f}s: {seen.get('session')} {seen.get('integrity')}")


def serial_log() -> Path:
    """The guest's serial log of this launch: run_qemu.py writes it beside the driver's output directory
    (`<run>/stage-<tag>/launch-N/serial.log` for `<run>/stage-<tag>-driver`)."""
    return out.parent / out.name.removesuffix("-driver") / f"launch-{os.environ.get('DEVVM_LAUNCH', '1')}" / "serial.log"


def serial_text() -> str:
    try:
        return serial_log().read_text(encoding="utf-8", errors="replace")
    except OSError:  # QEMU holds it open; the next look gets it
        return ""


def wait_serial_lines(marker: str, count: int) -> None:
    """Waits until `count` more lines containing `marker` have reached the serial log (the guest's watcher writes one
    every 10 s). The wait re-checks the log; the stage's own bound ends it if the guest has stopped writing."""
    target = serial_text().count(marker) + count
    while serial_text().count(marker) < target:
        time.sleep(1)  # pacing between looks at the log
