# /// script
# requires-python = ">=3.11"
# dependencies = ["paramiko", "pillow"]
# ///
"""Spike only (never merged): the host side of stage `ptywin`, run once per launch (run_qemu.py `--driver`; the launch
number is `DEVVM_LAUNCH`). Two launches on one disk: launch 1 is the first boot (`ssh-broker apply` starts the agent),
launch 2 the boot after a guest restart (the logon trigger starts it).

The release gate for the agent's console window (ssh-broker#63, the agent calls FreeConsole when it alone owns its
console). Per launch: wait until the agent serves (session 1, Medium); note the guest time; wait until the guest's
`winwatch` (a helper in session 1) has sampled the windows after it; run one no-pty control command (the agent answers); wait
for another sample; copy the evidence out and apply the pass rule (winverdict.py) to the window log. The rule: after the
agent serves, no visible window of the agent's console (no CASCADIA_HOSTING_WINDOW_CLASS titled with ssh-broker, no visible
ConsoleWindowClass, no visible window owned by those), and the control command is answered. `verdict-<N>.txt` has the
result; the driver log line `DEVVM-GATE` says PASS or FAIL. A FAIL does not stop the run: the evidence is the point.

Files: `launch<N>-windows.log` (the top-level windows of session 1, as they changed), `launch<N>-proc-events.txt` (every
process started and ended since the boot), `launch<N>-broker-logs-<file>`, `launch<N>-sshd-<file>`,
`launch<N>-terminal-facts.txt`, `desktop-<N>-agent-up.png` and `desktop-<N>-end.png` (the screen).

Exit codes are run_qemu.py's: 75 relaunch, 0 done (power down).

Usage: ptywin_driver.py <out dir> <key dir> <port>
"""

import os
import sys
import time
from datetime import datetime, timezone

from PIL import Image

import m5_pty
import paramiko_lib
import ssh_lib
import winverdict
from ssh_lib import in_session, probe_until, say, ssh

RELAUNCH = 75
LAUNCH = int(os.environ["DEVVM_LAUNCH"])
MEDIUM = in_session("1", "S-1-16-8192")
FACTS = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\terminal-facts.ps1"
BROKER_LOGS = r"C:\Users\user\AppData\Local\ssh-broker\logs"
SSHD_LOGS = r"C:\Users\Public\devvm-sshd-logs"
BEAT = r"C:\Users\Public\devvm-tracer-beat.txt"
WIN_BEAT = r"C:\ProgramData\devvm\win\beat.txt"
TICKS_AT_EPOCH = 621355968000000000

ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], f"driver-{LAUNCH}.log")
sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
begin = time.time()


def guest_ticks() -> int:
    _, out, _ = ssh("powershell.exe -NoProfile -Command [DateTime]::UtcNow.Ticks")
    return int(out.strip())


def iso(ticks: int) -> str:
    return datetime.fromtimestamp((ticks - TICKS_AT_EPOCH) / 1e7, tz=timezone.utc).isoformat()


def read(command: str) -> bytes:
    _, out, _ = paramiko_lib.exec_raw(command)
    return out


def save(name: str, data: bytes) -> None:
    (ssh_lib.out / f"launch{LAUNCH}-{name}").write_bytes(data)
    say(f"saved launch{LAUNCH}-{name} ({len(data)} bytes)")


def desktop(label: str) -> None:
    """Copies the next screen the runner writes (`last.png`, every 15 s) once it is newer than now. The wait re-checks the
    file; a half-written PNG fails to load and is looked at again."""
    last = ssh_lib.serial_log().parent / "last.png"
    mark = time.time()
    while True:
        if last.exists() and last.stat().st_mtime > mark:
            try:
                with Image.open(last) as image:
                    image.load()
                    image.save(ssh_lib.out / f"desktop-{LAUNCH}-{label}.png")
                say(f"desktop-{LAUNCH}-{label}.png saved {time.time() - mark:.1f}s after it was asked for")
                return
            except (OSError, SyntaxError, ValueError):
                pass  # the runner is writing it
        time.sleep(1)  # pacing between looks at the file


def control(label: str) -> bool:
    """One exec request without a pty that writes a marker file: the agent serves a command. A failure is a result."""
    name = f"{label}-l{LAUNCH}"
    say(f"== control {label}: guest time {iso(guest_ticks())}")
    try:
        status, out, err = paramiko_lib.exec_raw(m5_pty.marker_script(name), pty=False)
    except Exception as error:  # a result too: the log says what the client did
        say(f"control {name}: the client failed: {type(error).__name__}: {error}")
        return False
    (ssh_lib.out / f"control-{name}.raw").write_bytes(out)
    marker = m5_pty.read_marker(name)
    say(f"control {name}: status={status} stdout={out[-80:]!r} stderr={err[:80]!r} marker file: {marker!r}")
    return status == 0 and b"PTYEXEC-MARKER" in out and marker.startswith("ran pid=")


def after_beat(beat_file: str, ticks: int) -> None:
    """Waits until the guest helper that writes `beat_file` has begun a cycle after `ticks`: everything before `ticks` is in
    its files. The wait re-checks the file."""
    while True:
        beat = read(f"Get-Content -Raw -LiteralPath {beat_file}").strip()
        if beat.isdigit() and int(beat) > ticks:
            return
        time.sleep(1)  # pacing between looks at the beat


def collect() -> None:
    save("proc-events.txt", read(r"Get-Content -Raw -LiteralPath C:\Users\Public\devvm-proc-events.txt"))
    for folder, prefix in ((BROKER_LOGS, "broker-logs-"), (SSHD_LOGS, "sshd-")):
        names = read(f"Get-ChildItem -LiteralPath {folder} -Name").decode("utf-8", errors="replace").split()
        say(f"{folder}: {names}")
        for name in names:
            save(f"{prefix}{name}", read(f"Get-Content -Raw -LiteralPath {folder}\\{name}"))
    save("windows.log", read(r"Get-Content -Raw -LiteralPath C:\ProgramData\devvm\win\windows.log"))


say("waiting for SSH, then for the agent (session 1, Medium)")
first = probe_until("window", MEDIUM)
served = guest_ticks()
say(f"agent session reached {time.time() - begin:.1f}s into the launch; agents={first.get('agents')}; it served before guest time {iso(served)}")
desktop("agent-up")
code, out, err = ssh(FACTS)
save("terminal-facts.txt", out + err)

after_beat(WIN_BEAT, served)
say("winwatch has sampled after the agent served")
answered = control("control")
settled = guest_ticks()
say(f"control answered: {answered}; it ended before guest time {iso(settled)}; waiting for the watchers' next cycles")
after_beat(WIN_BEAT, settled)
after_beat(BEAT, settled)
collect()
desktop("end")

windows = (ssh_lib.out / f"launch{LAUNCH}-windows.log").read_text(encoding="utf-8", errors="replace")
passed, report = winverdict.verdict(windows, served)
passed = passed and answered
report += f"\nthe agent answered a command after the window check: {answered}"
(ssh_lib.out / f"verdict-{LAUNCH}.txt").write_text(report + "\n", encoding="utf-8")
say(f"DEVVM-GATE launch {LAUNCH}: {'PASS' if passed else 'FAIL'}")
for line in report.splitlines():
    say(f"verdict: {line}")

if LAUNCH == 1:
    code, _, _ = ssh("shutdown.exe /r /t 0")
    say(f"shutdown /r /t 0 issued (exit {code}): QEMU should exit by itself; launch 2 follows")
    sys.exit(RELAUNCH)
say("done")
sys.exit(0)
