"""Spike only (never merged): what the host side of every SSH-driven stage shares. `init` once, then `say`, `ssh`,
`probe` and `probe_until`. The log is `<out>/<log name>`; the probe is `probe.ps1` on the guest (`key=value` lines).
"""

import subprocess
import sys
import time
from collections.abc import Callable
from pathlib import Path

SSH = r"C:\Windows\System32\OpenSSH\ssh.exe"
PROBE = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\probe.ps1"
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


def ssh(command: str, known_hosts: str = "known_hosts", stdin: bytes | None = None) -> tuple[int, bytes, bytes]:
    args = [
        SSH, "-p", port, "-i", (keys / "client").as_posix(), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=yes", "-o", f"UserKnownHostsFile={(keys / known_hosts).as_posix()}", "-o", "ConnectTimeout=10",
        "user@127.0.0.1", command,
    ]  # fmt: skip
    done = subprocess.run(args, input=stdin if stdin is not None else b"", capture_output=True)
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
