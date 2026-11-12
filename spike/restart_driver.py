# /// script
# requires-python = ">=3.11"
# ///
"""Spike only (never merged): the host side of stage `restart`, run once per launch (run_qemu.py `--driver`; the launch
number is `DEVVM_LAUNCH`). The guest boots three times on one disk; each launch's driver measures its boot and ends it
with the next restart. Results go to `<out>/driver-<launch>.log`.

  launch 1  (`-no-reboot`)  first boot: the agent, the boot id. Then `shutdown /r`: QEMU must exit by itself (M8 a).
  launch 2  (`-no-reboot`)  boot after a guest restart: sshd (Automatic) before the logon, so the window (M4) is the probes before
                            session 1 answers; a new boot id; the pinned host key still accepted. Then `shutdown /s`.
  launch 3  (in place)      boot after a stop/start (M15's `LastBootUpTime` changes, the host key again, the window
                            again). Then a guest restart without `-no-reboot` (M8 b), then a QMP `system_reset` (M8 b).

Exit codes are run_qemu.py's: 75 relaunch, 76 relaunch without `-no-reboot`, 0 done (power down).

Usage: restart_driver.py <out dir> <key dir> <port>
"""

import json
import os
import sys
import time

import ssh_lib
from ssh_lib import in_session, probe_until, say, ssh

RELAUNCH = 75
RELAUNCH_IN_PLACE = 76
LAUNCH = int(os.environ["DEVVM_LAUNCH"])
MEDIUM = in_session("1", "S-1-16-8192")

ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], f"driver-{LAUNCH}.log")
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


def finish(code: int) -> None:
    say(f"driver ends with {code}")
    sys.exit(code)


if LAUNCH == 1:
    first = first_answers("boot 1")
    remember("boot1", first["boot"])
    code, _, err = ssh("shutdown.exe /r /t 0")
    say(f"shutdown /r /t 0 issued (exit {code}): QEMU should now exit by itself (-no-reboot)")
    finish(RELAUNCH)

elif LAUNCH == 2:
    second = first_answers("boot 2 (after a guest restart)")
    remember("boot2", second["boot"])
    say(f"M8 a: the boot id changed across the guest restart: {second['boot'] != boots().get('boot1')} ({boots().get('boot1')} -> {second['boot']})")
    code, _, err = ssh("shutdown.exe /s /t 0")
    say(f"shutdown /s /t 0 issued (exit {code}): QEMU should exit by itself")
    finish(RELAUNCH_IN_PLACE)

else:
    third = first_answers("boot 3 (after a stop and start)")
    remember("boot3", third["boot"])
    say(f"M15: LastBootUpTime ticks changed across stop/start: {third['boot'] != boots().get('boot2')} ({boots().get('boot2')} -> {third['boot']})")

    # M8 b, the guest restarts and QEMU does not exit (no -no-reboot): does Windows come back with the agent?
    code, _, err = ssh("shutdown.exe /r /t 0")
    say(f"M8 b: shutdown /r /t 0 issued (exit {code}); waiting for a new boot id in session 1")
    fourth = probe_until("in-place restart", new_boot(third["boot"]))
    remember("boot4", fourth["boot"])
    say(f"M8 b: after the guest restart in place: session 1 Medium, boot {fourth['boot']} ({time.time() - begin:.1f}s into the launch)")

    # M8 b, a hard reset from the host: QMP system_reset.
    print("QMP:system_reset", flush=True)
    say("M8 b: QMP system_reset asked for; waiting for a new boot id in session 1")
    fifth = probe_until("after system_reset", new_boot(fourth["boot"]))
    remember("boot5", fifth["boot"])
    say(f"M8 b: after system_reset: session 1 Medium, boot {fifth['boot']} ({time.time() - begin:.1f}s into the launch)")
    finish(0)
