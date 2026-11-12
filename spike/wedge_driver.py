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
  echo       a TCP echo server on port 22 behind the same QEMU forward, no sshd, pipes or shell (stages/wedge/devvm-echo.cs);
             this driver sends 1 MiB and reads it back, SAMPLES times
  local      the guest runs the shim itself, with pipes, no sshd and no network (stages/wedge/stage-user.ps1); this driver
             only waits for it

The variant name may carry options after dots (`medium.b95`; stages/wedge/stage-system.ps1 lists them): a different sshd
(`b95` 9.5.0.0p1-Beta, `p98` 9.8.3.0p2-Preview, `p100` 10.0.0.0p2-Preview, `inbox` Windows' own) or sshd's log level.

Lines `WEDGE <variant> <payload> #n: PASS|FAIL|HANG ...` in `<out>/driver.log`. A run that did not end in
paramiko_lib.COMMAND_BOUND seconds is a HANG (the line says how many bytes went each way); the payload's samples stop
there. If sshd then gives no banner the driver waits for three watcher lines (the guest's state after the client left)
and ends QEMU (`QMP:quit`).

Usage: wedge_driver.py <out dir> <key dir> <port>
"""

import socket
import sys
import threading
import time
from pathlib import Path

import m5_battery as b
import paramiko_lib
import ssh_lib
from ssh_lib import in_session, probe_until, say, switch_agent

ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], "driver.log")
sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
b.init()

FULL = ssh_lib.out.name.removeprefix("stage-wedge-").removesuffix("-driver")
VARIANT, *OPTIONS = FULL.split(".")
SSHD = next((option for option in OPTIONS if option in ("b95", "p98", "p100", "inbox")), None)
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


say(f"variant {FULL}")
if SSHD:
    # The guest installs the other sshd first (stages/wedge/stage-system.ps1) and says how that went. A failed install
    # leaves no sshd to probe, so the variant ends here with the guest's account in the serial log.
    while "DEVVM-STEP-SSHD-VARIANT=" not in ssh_lib.serial_text():
        time.sleep(1)  # pacing between looks at the serial log
    if "DEVVM-STEP-SSHD-VARIANT=ok" not in ssh_lib.serial_text():
        say(f"WEDGE {FULL}: the sshd {SSHD} did not install (the serial log has the guest's account)")
        print("QMP:quit", flush=True)
        end(0)
    say(f"sshd {SSHD} installed")
if VARIANT == "local":
    say("waiting for the guest's own harness (no SSH in its path)")
    while "DEVVM-WEDGE-LOCAL-DONE" not in ssh_lib.serial_text():
        time.sleep(1)  # pacing between looks at the serial log
    ssh_lib.wait_serial_lines(WATCH, 2)
    end(0)

def echo_once(data: bytes) -> tuple[bool, int, int, float]:
    """One connection to the guest's echo server: all of `data` sent in 32 KiB writes while the same thread-pair reads the
    echo. The socket's timeout (COMMAND_BOUND) bounds a connect or a recv that never comes. Returns (ok, bytes sent,
    bytes echoed, seconds)."""
    began = time.monotonic()
    sock = socket.create_connection(("127.0.0.1", int(sys.argv[3])), timeout=paramiko_lib.COMMAND_BOUND)
    sent = [0]

    def feed() -> None:
        try:
            for start in range(0, len(data), paramiko_lib.CHUNK):
                sock.sendall(data[start:start + paramiko_lib.CHUNK])
                sent[0] = min(start + paramiko_lib.CHUNK, len(data))
        except OSError:  # the bound below closed the socket, or the guest reset it
            pass

    feeder = threading.Thread(target=feed)
    feeder.start()
    echoed = bytearray()
    try:
        while len(echoed) < len(data):
            chunk = sock.recv(65536)
            if not chunk:
                break
            echoed += chunk
    except OSError:  # TimeoutError: nothing came for COMMAND_BOUND seconds
        pass
    finally:
        sock.close()
        feeder.join()
    return bytes(echoed) == data, sent[0], len(echoed), time.monotonic() - began


if VARIANT == "echo":
    while "DEVVM-ECHO-READY" not in ssh_lib.serial_text():
        time.sleep(1)  # pacing between looks at the serial log
    data = b.megabytes(1)
    for n in range(1, SAMPLES + 1):
        ok, sent, echoed, took = echo_once(data)
        say(f"WEDGE echo #{n}: {'PASS' if ok else 'HANG'} sent={sent}/{len(data)} echoed={echoed}/{len(data)} took={took:.1f}s")
        if not ok:
            break
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
            say(f"WEDGE {FULL} {name} #{n}: sshd did not send its banner")
            end_wedged()
        ok = b.mask(status) == 0 and out == data
        hang = status == -2 and b"no exit status" in err
        say(f"WEDGE {FULL} {name} #{n}: {'PASS' if ok else 'HANG' if hang else 'FAIL'} status={status} "
            f"stdout={len(out)}/{len(data)} same_prefix={out == data[:len(out)]} err={b.show(err, 200)} took={b._took[0]:.1f}s")  # fmt: skip
        if not ok:
            if not paramiko_lib.reachable():
                end_wedged()
            break  # sshd still serves: the next payload starts on what the failure left behind

say("every payload ran")
ssh_lib.wait_serial_lines(WATCH, 2)
end(0)
