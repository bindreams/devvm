# /// script
# requires-python = ">=3.11"
# dependencies = ["paramiko", "pillow"]
# ///
"""Spike only (never merged): the host side of stage `ptywin`, run once per launch (run_qemu.py `--driver`; the launch
number is `DEVVM_LAUNCH`). Two launches on one disk: launch 1 is the first boot (`ssh-broker apply` starts the agent),
launch 2 the boot after a guest restart (the logon trigger starts it). Results go to `<out>/driver-<launch>.log`.

B1.3, an exec request with a pty never ran its command (a marker file the command writes was absent; 151 bytes of
pseudoconsole start and teardown, status 0). Per launch, five requests, each followed by a read of its marker file:
  nopty-pre   no pty: the control, the same marker script (known to work)
  pty-ps      paramiko, pty, the marker script (the request of runs 43278227165 and 43289660404)
  pty-cmd     paramiko, pty, `cmd /c "echo ran> file"` (no PowerShell in the command)
  pty-sshexe  the stock Windows `ssh.exe -tt`, the marker script (another client)
  nopty-post  the control again (the agent still works)
Then, once the guest's tracer (SYSTEM) has finished a cycle after the last request, the evidence is copied out:
`launch<N>-proc-events.txt` (every process started and ended since the boot: image, command line, parent, exit status),
`launch<N>-broker-logs-<file>` (the user's ssh-broker logs: shim.log, agent.log), `launch<N>-sshd-<file>` (sshd's own logs
at DEBUG3), and `launch<N>-windows.log` (the top-level windows of session 1, as they changed).

The visible-window baseline (before ssh-broker#63): `desktop-<N>-agent-up.png` and `desktop-<N>-end.png` are the screen,
`launch<N>-windows.log` lists the windows (from a helper in session 1), `launch<N>-terminal-facts.txt` the default
terminal settings and what hosts the agent's console.

Exit codes are run_qemu.py's: 75 relaunch, 0 done (power down).

Usage: ptywin_driver.py <out dir> <key dir> <port>
"""

import os
import subprocess
import sys
import time
from datetime import datetime, timezone

from PIL import Image

import m5_pty
import paramiko_lib
import ssh_lib
from ssh_lib import in_session, probe_until, say, ssh

RELAUNCH = 75
LAUNCH = int(os.environ["DEVVM_LAUNCH"])
MEDIUM = in_session("1", "S-1-16-8192")
FACTS = r"powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\terminal-facts.ps1"
BROKER_LOGS = r"C:\Users\user\AppData\Local\ssh-broker\logs"
SSHD_LOGS = r"C:\Users\Public\devvm-sshd-logs"
BEAT = r"C:\Users\Public\devvm-tracer-beat.txt"
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


def request(label: str, command: str | None, pty: bool, client: str = "paramiko") -> None:
    """One exec request and its marker file. A failure of the request is a result, not the end of the driver."""
    name = f"{label}-l{LAUNCH}"
    before = guest_ticks()
    say(f"== request {label}: guest time {iso(before)}")
    try:
        if client == "paramiko":
            m5_pty.exec_marker(name, command, pty=pty)
        else:  # the stock Windows client
            script = command or m5_pty.marker_script(name)
            status, out, err = ssh(script, tty=True, bound=paramiko_lib.COMMAND_BOUND)
            (ssh_lib.out / f"pty-exec-{name}.raw").write_bytes(out)
            say(f"pty exec {name} (ssh.exe -tt): status={status} stdout={out[-80:]!r} ({len(out)} bytes) stderr={err[:200]!r}")
            say(f"pty exec {name}: marker file (read without a pty): {m5_pty.read_marker(name)!r}")
    except (subprocess.TimeoutExpired, TimeoutError, paramiko_lib.Wedged) as error:
        say(f"pty exec {name}: no result: {type(error).__name__}: {error}")
    except Exception as error:  # a result too: the log says what the client did
        say(f"pty exec {name}: the client failed: {type(error).__name__}: {error}")
    say(f"== request {label} done: guest time {iso(guest_ticks())}")


def after_tracer(ticks: int) -> None:
    """Waits until the guest's tracer has begun a cycle after `ticks`: everything before `ticks` is in its files."""
    while True:
        beat = read(f"Get-Content -Raw -LiteralPath {BEAT}").strip()
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
say(f"agent session reached {time.time() - begin:.1f}s into the launch; agents={first.get('agents')}")
desktop("agent-up")
code, out, err = ssh(FACTS)
save("terminal-facts.txt", out + err)

request("nopty-pre", None, pty=False)
request("pty-ps", None, pty=True)
cmd_marker = m5_pty.marker_path(f"pty-cmd-l{LAUNCH}")
request("pty-cmd", f'cmd /c "echo ran> {cmd_marker}"', pty=True)
request("pty-sshexe", None, pty=True, client="ssh.exe")
request("nopty-post", None, pty=False)

settled = guest_ticks()
say(f"the last request ended before guest time {iso(settled)}; waiting for the tracer's next cycle")
after_tracer(settled)
collect()
desktop("end")

if LAUNCH == 1:
    code, _, _ = ssh("shutdown.exe /r /t 0")
    say(f"shutdown /r /t 0 issued (exit {code}): QEMU should exit by itself; launch 2 follows")
    sys.exit(RELAUNCH)
say("done")
sys.exit(0)
