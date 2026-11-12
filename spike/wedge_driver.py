# /// script
# requires-python = ">=3.11"
# dependencies = ["paramiko"]
# ///
"""Spike only (never merged): the host side of stage `wedge`, one variant per run (the stage's variant.txt; the driver
reads it from its output directory's name). Run 43260908167: a command with 1 MiB on stdin did not end (the client had
sent it all and had 196 to 262 KB back), and afterwards sshd accepted connections but never spawned a session. sshd,
the broker's relay and QEMU's forward were the candidates. Each variant runs the same payloads (guest/stages/wedge/
payload-*.ps1, the program `emit cat` as the wrapper would call it) with 1 MiB on stdin, SAMPLES times each:

  nobroker   sshd's DefaultShell is pwsh (no broker): sshd -> pwsh -> emit
  medium     sshd -> broker shim -> Medium agent -> pwsh -> emit
  elevated   the same through the elevated agent
  local      the guest runs the shim itself, with pipes, no sshd and no network (stages/wedge/stage-user.ps1); this driver
             only waits for it

Lines `WEDGE <variant> <payload> #n: PASS|FAIL|HANG ...` in `<out>/driver.log`. A run that did not end in
paramiko_lib.COMMAND_BOUND seconds is a HANG (the line says how many bytes went each way); the payload's samples stop
there. If sshd then gives no banner the driver waits for three watcher lines (the guest's state after the client left)
and ends QEMU (`QMP:quit`).

Usage: wedge_driver.py <out dir> <key dir> <port>
"""

import sys
import time
from pathlib import Path

import m5_battery as b
import paramiko_lib
import ssh_lib
from ssh_lib import in_session, probe_until, say, switch_agent

ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], "driver.log")
sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
b.init()

VARIANT = ssh_lib.out.name.removeprefix("stage-wedge-").removesuffix("-driver")
# The number of runs per payload is the sample size of the measurement, not a retry limit.
SAMPLES = 20
STAGE = Path(__file__).parent / "guest" / "stages" / "wedge"
PAYLOADS = {name: (STAGE / f"payload-{name}.ps1").read_text(encoding="utf-8") for name in ("plain", "c1", "c2")}
WATCH = "DEVVM-WATCH t="


def end(code: int) -> None:
    say(f"driver ends with {code}")
    sys.exit(code)


def end_wedged() -> None:
    """sshd serves nobody: the guest's own watcher says what the guest looks like now; then QEMU ends."""
    b.note_wedge()
    say("sshd does not send its banner: waiting for three watcher lines, then ending QEMU")
    ssh_lib.wait_serial_lines(WATCH, 3)
    print("QMP:quit", flush=True)
    end(0)


say(f"variant {VARIANT}")
if VARIANT == "local":
    say("waiting for the guest's own harness (no SSH in its path)")
    while "DEVVM-WEDGE-LOCAL-DONE" not in ssh_lib.serial_text():
        time.sleep(1)  # pacing between looks at the serial log
    ssh_lib.wait_serial_lines(WATCH, 2)
    end(0)

if VARIANT == "nobroker":
    probe_until("window", lambda seen: True)  # sshd answers; pwsh runs the probe (session 0, no agent in the way)
else:
    probe_until("window", in_session("1", "S-1-16-8192"))
    if VARIANT == "elevated":
        switch_agent("E-C", "devvm-agent-elevated", in_session("1", "S-1-16-12288"))

data = b.megabytes(1)
names = ("plain",) if VARIANT == "nobroker" else ("plain", "c1", "c2")
for name in names:
    for n in range(1, SAMPLES + 1):
        try:
            status, out, err = b.run_script(PAYLOADS[name], data)
        except paramiko_lib.Wedged:
            say(f"WEDGE {VARIANT} {name} #{n}: sshd did not send its banner")
            end_wedged()
        ok = b.mask(status) == 0 and out == data
        hang = status == -2 and b"no exit status" in err
        say(f"WEDGE {VARIANT} {name} #{n}: {'PASS' if ok else 'HANG' if hang else 'FAIL'} status={status} "
            f"stdout={len(out)}/{len(data)} same_prefix={out == data[:len(out)]} err={b.show(err, 200)} took={b._took[0]:.1f}s")  # fmt: skip
        if not ok:
            if not paramiko_lib.reachable():
                end_wedged()
            break  # sshd still serves: the next payload starts on what the failure left behind

say("every payload ran")
ssh_lib.wait_serial_lines(WATCH, 2)
end(0)
