# /// script
# requires-python = ">=3.11"
# dependencies = ["paramiko"]
# ///
"""Spike only (never merged): the host side of stage `restart`, run once per launch (run_qemu.py `--driver`; the launch
number is `DEVVM_LAUNCH`). The guest boots three times on one disk; each launch's driver measures its boot and ends it
with the next restart. Results go to `<out>/driver-<launch>.log`.

  launch 1  (`-no-reboot`)  first boot: the agent, the boot id. Then `shutdown /r`: QEMU must exit by itself (M8 a).
  launch 2  (`-no-reboot`)  boot after a guest restart: sshd (Automatic) before the logon, so the window (M4) is the probes before
                            session 1 answers; a new boot id; the pinned host key still accepted. Then `shutdown /s`.
  launch 3  (in place)      boot after a stop/start (M15's `LastBootUpTime` changes, the host key again, the window
                            again). Then a guest restart without `-no-reboot` (M8 b), then a QMP `system_reset` (M8 b).

Variant `eb` (stage `restart:eb`, M6): after every boot, with nobody acting (the guest registered the helpers once, on
the first boot), the elevated helper is used from the Medium agent: E-B as measured (`EB`, one `current.txt`) and with one
request per start (`EBP`), each once for the High probe and once for a command; then two `EBP` requests at once, which
wait for each other's barrier file, so both pass only if both instances run at the same time. Lines `M6 ...` in the log.

Exit codes are run_qemu.py's: 75 relaunch, 76 relaunch without `-no-reboot`, 0 done (power down).

Usage: restart_driver.py <out dir> <key dir> <port>
"""

import json
import os
import sys
import time
import uuid
from concurrent.futures import ThreadPoolExecutor

import m5_battery as b
import m5_carriers as cr
import paramiko_lib
import ssh_lib
from ssh_lib import in_session, probe_until, say, ssh

RELAUNCH = 75
RELAUNCH_IN_PLACE = 76
LAUNCH = int(os.environ["DEVVM_LAUNCH"])
MEDIUM = in_session("1", "S-1-16-8192")

ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], f"driver-{LAUNCH}.log")
b.init()
EB = ssh_lib.out.name.removeprefix("stage-restart").removesuffix("-driver").lstrip("-") == "eb"
sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
BOOTS = ssh_lib.out / "boot-ids.json"
begin = time.time()


def boots() -> dict[str, str]:
    return json.loads(BOOTS.read_text(encoding="utf-8")) if BOOTS.exists() else {}


def remember(label: str, boot: str) -> None:
    known = boots()
    known[label] = boot
    BOOTS.write_text(json.dumps(known, indent=1), encoding="utf-8")


def new_boot(previous: str):
    return lambda seen: MEDIUM(seen) and seen.get("boot") not in (None, "", previous)


def first_answers(label: str) -> dict[str, str]:
    """Probes until session 1 (Medium) answers; every probe before it is a line of the window."""
    say(f"{label}: probing from the start of the launch (sshd starts with the service manager on boots 2 and later)")
    seen = probe_until("window", MEDIUM)
    say(f"{label}: agent session (1, Medium) answered {time.time() - begin:.1f}s after the driver started; boot={seen.get('boot')}")
    return seen


def pwsh(script: str) -> list[str]:
    return [cr.PWSH, "-NoProfile", "-NonInteractive", "-Command", script]


def run(carrier: str, argv: list[str]) -> tuple[int, bytes, bytes]:
    """b.run, with sshd not answering reported as a failed check (status -2) instead of ending the driver."""
    try:
        return b.run(carrier, argv)
    except paramiko_lib.Wedged as error:
        return -2, b"", f"sshd did not answer: {error}".encode()


def eb_single(label: str, carrier: str) -> None:
    """One High probe and one command through `carrier`: the helper must still be there and run as High in session 1."""
    status, out, err = run(carrier, b.PROBE)
    seen = ssh_lib.facts(out)
    high = (seen.get("session"), seen.get("integrity")) == ("1", "S-1-16-12288")
    took = b._took[0]
    status2, out2, err2 = run(carrier, pwsh("[Console]::Out.Write('eb-check')"))
    ok = high and b.mask(status) == 0 and b.mask(status2) == 0 and out2 == b"eb-check"
    say(f"M6 {label} [{carrier}]: {'PASS' if ok else 'FAIL'} probe status={status} session={seen.get('session')} "
        f"integrity={seen.get('integrity')} elevated={seen.get('elevated')} took={took:.1f}s; command status={status2} "
        f"out={b.show(out2)} err={b.show(err2 or err)} took={b._took[0]:.1f}s")  # fmt: skip


def eb_concurrent(label: str) -> None:
    """Two `EBP` requests at once. Each creates its own barrier file and waits for the other's, then answers with its own
    marker and exit code: a dropped request leaves the other waiting (the client's bound ends it), a request that ran
    the other's script would answer with the wrong marker."""
    tag = uuid.uuid4().hex[:8]
    prefix = rf"C:\ProgramData\devvm\eb\barrier-{tag}"

    def request(me: str, other: str, code: int) -> tuple[int, bytes, bytes]:
        script = (f"$d = '{prefix}'; New-Item -ItemType File -Force -Path \"$d-{me}\" | Out-Null; "
                  f"while (-not (Test-Path \"$d-{other}\")) {{ Start-Sleep -Milliseconds 100 }}; "
                  f"[Console]::Out.Write('answer-{me}-{tag}'); exit {code}")  # fmt: skip
        try:
            return paramiko_lib.exec_raw(cr.CARRIERS["EBP"](pwsh(script)))
        except Exception as error:  # a bound or a wedged sshd is the result, not the end of the driver
            return -2, b"", f"{type(error).__name__}: {error}".encode()

    began = time.monotonic()
    with ThreadPoolExecutor(max_workers=2) as pool:
        first = pool.submit(request, "A", "B", 3)
        second = pool.submit(request, "B", "A", 5)
        (status_a, out_a, err_a), (status_b, out_b, err_b) = first.result(), second.result()
    ok = (b.mask(status_a), out_a) == (3, f"answer-A-{tag}".encode()) and (b.mask(status_b), out_b) == (5, f"answer-B-{tag}".encode())
    say(f"M6 {label} [two EBP requests at once]: {'PASS' if ok else 'FAIL'} A status={status_a} out={b.show(out_a)} "
        f"err={b.show(err_a)}; B status={status_b} out={b.show(out_b)} err={b.show(err_b)}; took={time.monotonic() - began:.1f}s")  # fmt: skip


def eb_round(label: str) -> None:
    if not EB:
        return
    say(f"M6 {label}: the elevated helper, first use in this boot, nobody acting")
    eb_single(label, "EB")
    eb_single(label, "EBP")
    eb_concurrent(label)


def finish(code: int) -> None:
    say(f"driver ends with {code}")
    sys.exit(code)


if LAUNCH == 1:
    first = first_answers("boot 1")
    remember("boot1", first["boot"])
    eb_round("boot 1")
    code, _, err = ssh("shutdown.exe /r /t 0")
    say(f"shutdown /r /t 0 issued (exit {code}): QEMU should now exit by itself (-no-reboot)")
    finish(RELAUNCH)

elif LAUNCH == 2:
    second = first_answers("boot 2 (after a guest restart)")
    remember("boot2", second["boot"])
    eb_round("boot 2 (after a guest restart)")
    say(f"M8 a: the boot id changed across the guest restart: {second['boot'] != boots().get('boot1')} ({boots().get('boot1')} -> {second['boot']})")
    code, _, err = ssh("shutdown.exe /s /t 0")
    say(f"shutdown /s /t 0 issued (exit {code}): QEMU should exit by itself")
    finish(RELAUNCH_IN_PLACE)

else:
    third = first_answers("boot 3 (after a stop and start)")
    remember("boot3", third["boot"])
    eb_round("boot 3 (after a stop and start)")
    say(f"M15: LastBootUpTime ticks changed across stop/start: {third['boot'] != boots().get('boot2')} ({boots().get('boot2')} -> {third['boot']})")

    # M8 b, the guest restarts and QEMU does not exit (no -no-reboot): does Windows come back with the agent?
    code, _, err = ssh("shutdown.exe /r /t 0")
    say(f"M8 b: shutdown /r /t 0 issued (exit {code}); waiting for a new boot id in session 1")
    fourth = probe_until("in-place restart", new_boot(third["boot"]))
    remember("boot4", fourth["boot"])
    eb_round("boot 4 (after a restart in place)")
    say(f"M8 b: after the guest restart in place: session 1 Medium, boot {fourth['boot']} ({time.time() - begin:.1f}s into the launch)")

    # M8 b, a hard reset from the host: QMP system_reset.
    print("QMP:system_reset", flush=True)
    say("M8 b: QMP system_reset asked for; waiting for a new boot id in session 1")
    fifth = probe_until("after system_reset", new_boot(fourth["boot"]))
    remember("boot5", fifth["boot"])
    eb_round("boot 5 (after system_reset)")
    say(f"M8 b: after system_reset: session 1 Medium, boot {fifth['boot']} ({time.time() - begin:.1f}s into the launch)")
    finish(0)
