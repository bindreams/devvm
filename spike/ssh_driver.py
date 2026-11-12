# /// script
# requires-python = ">=3.11"
# ///
"""Spike only (never merged): the host side of stage `ssh`. Runs beside the guest (run_qemu.py `--driver`) and measures
what needs SSH into it: M3 (the pinned host key), M4 (when the agent exists, what a command runs as before it, `verify`),
M6 E0 (a) (the session when the agent is gone) and E-C (an elevated agent). Every result goes to `<out>/driver.log`.

Usage: ssh_driver.py <out dir> <key dir> <port>
  <key dir> holds `client` (private key), `known_hosts` (the pinned host key), `known_hosts_other` (a different key).
"""

import subprocess
import sys
import time
from pathlib import Path

SSH = r"C:\Windows\System32\OpenSSH\ssh.exe"
PROBE = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\probe.ps1"
KILL = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\killagent.ps1"
BROKER = r"C:\ProgramData\ssh-broker\ssh-broker.exe"
# A failure of these kinds is the answer, not "not yet".
FATAL = ("REMOTE HOST IDENTIFICATION HAS CHANGED", "Host key verification failed", "Permission denied")
SESSION_ONE_MEDIUM = ("1", "S-1-16-8192")
SESSION_ONE_HIGH = ("1", "S-1-16-12288")

out = Path(sys.argv[1])
keys = Path(sys.argv[2])
port = sys.argv[3]
out.mkdir(parents=True, exist_ok=True)
log = (out / "driver.log").open("w", encoding="utf-8", buffering=1)
begin = time.time()


def say(text: str) -> None:
    log.write(f"[{time.time() - begin:8.1f}s] {text}\n")


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
        key, _, value = line.partition("=")
        if _:
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
        f"elevated={seen.get('elevated')} hkcu={seen.get('hkcu')} dpapi={seen.get('dpapi')} parent={seen.get('parent')}")
    return seen


def probe_until(label: str, wanted: tuple[str, str]) -> dict[str, str]:
    """Probes until the session is `wanted` (session id, integrity). The wait re-checks the condition; no cap."""
    while True:
        seen = probe(label)
        if seen is not None and (seen.get("session"), seen.get("integrity")) == wanted:
            return seen
        time.sleep(1)  # pacing between probes


def record(label: str, command: str) -> int:
    code, stdout, stderr = ssh(command)
    say(f"{label}: exit {code}")
    (out / f"{label}.stdout.txt").write_bytes(stdout)
    (out / f"{label}.stderr.txt").write_bytes(stderr)
    return code


# M3 and M4: from the first answer, probe until the agent's session (1, Medium) shows. The probes before it are the window.
say("waiting for SSH, then for the agent")
first = probe_until("window", SESSION_ONE_MEDIUM)
say(f"agent session reached after {time.time() - begin:.1f}s of the run")
(out / "medium-probe.txt").write_text("\n".join(f"{k}={v}" for k, v in first.items()) + "\n", encoding="utf-8")

# M3: the pinned key is the one the guest serves; a different key is refused.
code, _, stderr = ssh("exit 0", known_hosts="known_hosts_other")
say(f"M3 other host key: exit {code}: {stderr.decode('utf-8', errors='replace').strip()[:300]}")

# M4: verify, the shell config and the facts the agent session reports.
record("verify", f"{BROKER} verify")
record("reg-openssh", r"reg query HKLM\SOFTWARE\OpenSSH")

# M6 E0 (a): the agent gone, what does the failing-open session report?
record("killagent", KILL)
plain = probe("E0a-after-kill")
if plain is not None:
    (out / "e0a-probe.txt").write_text("\n".join(f"{k}={v}" for k, v in plain.items()) + "\n", encoding="utf-8")

# M6 E-C: an elevated agent started from a highest-privilege task.
started = record("start-elevated-agent", 'powershell.exe -NoProfile -Command "Start-ScheduledTask -TaskName devvm-agent-elevated"')
if started == 0:
    elevated = probe_until("E-C", SESSION_ONE_HIGH)
    (out / "ec-probe.txt").write_text("\n".join(f"{k}={v}" for k, v in elevated.items()) + "\n", encoding="utf-8")
    record("ec-verify", f"{BROKER} verify")
else:
    say("E-C: the task could not be started from the plain session; no elevated agent")
say("done")
