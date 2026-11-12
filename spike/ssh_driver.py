# /// script
# requires-python = ">=3.11"
# ///
"""Spike only (never merged): the host side of stage `ssh`. Runs beside the guest (run_qemu.py `--driver`) and measures
what needs SSH into it: M3 (the pinned host key), M4 (when the agent exists, what a command runs as before it, `verify`),
M6 E0 (a) (the session when the agent is gone) and E-C (an elevated agent). Every result goes to `<out>/driver.log`.

Usage: ssh_driver.py <out dir> <key dir> <port>
  <key dir> holds `client` (private key), `known_hosts` (the pinned host key), `known_hosts_other` (a different key).
"""

import sys
import time

import ssh_lib
from ssh_lib import in_session, probe, probe_until, say, ssh

KILL = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\killagent.ps1"
BROKER = r"C:\ProgramData\ssh-broker\ssh-broker.exe"
SESSION_ONE_MEDIUM = in_session("1", "S-1-16-8192")
SESSION_ONE_HIGH = in_session("1", "S-1-16-12288")

sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], "driver.log")
begin = time.time()


def record(label: str, command: str) -> int:
    code, stdout, stderr = ssh(command)
    say(f"{label}: exit {code}")
    (ssh_lib.out / f"{label}.stdout.txt").write_bytes(stdout)
    (ssh_lib.out / f"{label}.stderr.txt").write_bytes(stderr)
    return code


# M3 and M4: from the first answer, probe until the agent's session (1, Medium) shows. The probes before it are the window.
say("waiting for SSH, then for the agent")
first = probe_until("window", SESSION_ONE_MEDIUM)
say(f"agent session reached after {time.time() - begin:.1f}s of the run")
(ssh_lib.out / "medium-probe.txt").write_text("\n".join(f"{k}={v}" for k, v in first.items()) + "\n", encoding="utf-8")

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
    (ssh_lib.out / "e0a-probe.txt").write_text("\n".join(f"{k}={v}" for k, v in plain.items()) + "\n", encoding="utf-8")

# M6 E-C: an elevated agent started from a highest-privilege task.
started = record("start-elevated-agent", 'powershell.exe -NoProfile -Command "Start-ScheduledTask -TaskName devvm-agent-elevated"')
if started == 0:
    elevated = probe_until("E-C", SESSION_ONE_HIGH)
    (ssh_lib.out / "ec-probe.txt").write_text("\n".join(f"{k}={v}" for k, v in elevated.items()) + "\n", encoding="utf-8")
    record("ec-verify", f"{BROKER} verify")
else:
    say("E-C: the task could not be started from the plain session; no elevated agent")
say("done")
