# /// script
# requires-python = ">=3.11"
# dependencies = ["paramiko"]
# ///
"""Spike only (never merged): the host side of stage `m5`. Runs beside the guest (run_qemu.py `--driver`) and measures
M5 (the wrapper battery through the broker and `pwsh -c`, carriers C1 and C2 of plan 6.5), M10 (the SFTP server), a pty
smoke, and the same battery through the two elevated candidates of M6: E-B (the elevated scheduled-task helper, started
from the Medium session) and E-C (an elevated agent). Results are lines in `<out>/driver.log`; `m5_battery.py` has the
tests. The 1 MiB and 4 MiB stdin tests run last, after the Medium agent is back: a command with a large stdin did not
end on either guest and sshd then stopped answering (run 43260908167); stage `wedge` isolates that.

Variant `lean` (stage `m5:lean`, the LTSC leg): without E-C and the large stdin tests. E-C is not a candidate any more,
and the large stdin tests wait for the remedy of the OpenSSH defects (stage `wedge`); everything else runs as before.

Usage: m5_driver.py <out dir> <key dir> <port>
"""

import sys
import time

import m5_battery as b
import m5_pty
import m10_sftp
import paramiko_lib
import ssh_lib
from ssh_lib import in_session, probe_until, say, switch_agent

ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], "driver.log")
sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
b.init()
LEAN = ssh_lib.out.name.removeprefix("stage-m5").removesuffix("-driver").lstrip("-") == "lean"

MEDIUM = ("C1", "C2")
ELEVATED_AGENT = ("C1/EC", "C2/EC")
HELPER = ("EB",)
AT_MEDIUM = in_session("1", "S-1-16-8192")
AT_HIGH = in_session("1", "S-1-16-12288")
# Padding sizes (characters added to the script) around the length where the command stopped arriving.
SIZES = (1_000, 8_000, 16_000, 30_000, 32_000, 32_100, 32_200, 32_300, 32_400, 32_500, 40_000, 130_000)


def elevated_battery(carriers: tuple[str, ...], integrity: str) -> None:
    b.section("M5 bytes and streams", lambda: b.bytes_and_streams(carriers))
    b.section("M5 exit codes", lambda: b.exit_codes(carriers, (0, 7, 255, 3221225477, -1)))
    b.section("M5 argv", lambda: b.argv_checks(carriers, b.HOSTILE_SHORT))
    b.section("M5 program names and cwd", lambda: b.names_and_cwd(carriers, full=False))
    b.section("M5 nested pwsh", lambda: b.nested_pwsh(carriers, full=False))
    b.section("M5 facts", lambda: b.facts(carriers, "1", integrity))


say("waiting for SSH, then for the agent")
begin = time.time()
probe_until("window", AT_MEDIUM)
say(f"agent session reached after {time.time() - begin:.1f}s")

# The Medium agent =====================================================================================================
b.section("M5 bytes and streams", lambda: b.bytes_and_streams(MEDIUM))
b.section("M5 exit codes", lambda: b.exit_codes(MEDIUM))
b.section("M5 argv", lambda: b.argv_checks(MEDIUM))
b.section("M5 program names and cwd", lambda: b.names_and_cwd(MEDIUM))
b.section("M5 cmd.exe and .cmd targets", b.cmd_targets)
b.section("M5 nested pwsh", lambda: b.nested_pwsh(MEDIUM))
b.section("M5 longest command", lambda: b.longest_command(MEDIUM, SIZES))
b.section("M5 facts", lambda: b.facts(MEDIUM, "1", "S-1-16-8192"))
b.section("M10 sftp", m10_sftp.measure)
b.section("pty smoke", m5_pty.measure)

# E-B: the elevated helper, called from the Medium agent's session. ====================================================
say("== E-B: the elevated scheduled-task helper")
elevated_battery(HELPER, "S-1-16-12288")

# E-C: the elevated agent. =============================================================================================
say("== E-C: the elevated agent")
if LEAN:
    say("E-C SKIPPED: variant lean")
elif paramiko_lib.reachable():
    switch_agent("E-C", "devvm-agent-elevated", AT_HIGH)
    elevated_battery(ELEVATED_AGENT, "S-1-16-12288")
    b.section("M5 longest command", lambda: b.longest_command(ELEVATED_AGENT, (16_000, 32_000)))
    if paramiko_lib.reachable():
        switch_agent("Medium again", "devvm-agent-medium", AT_MEDIUM)
    else:
        say("Medium again SKIPPED: sshd does not send its banner")
else:
    say("E-C SKIPPED: sshd does not send its banner")

# The large stdin tests, last: one of them can leave sshd not answering, and nothing after it could run. ===============
# E-C's large stdin is stage `wedge`'s elevated variant.
if LEAN:
    say("M5 stdin 1 MiB and 4 MiB SKIPPED: variant lean")
else:
    b.section("M5 stdin 1 MiB", lambda: b.stdin_large(MEDIUM + HELPER, 1))
    b.section("M5 stdin 4 MiB", lambda: b.stdin_large(MEDIUM + HELPER, 4))

b.summary()
say("done")
if not paramiko_lib.reachable():
    print("QMP:quit", flush=True)  # a guest with a wedged sshd may not shut down: end QEMU
