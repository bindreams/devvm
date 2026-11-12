"""Spike only (never merged): the M5 tests and what they share. `m5_driver.py` runs them through each carrier (the
Medium agent, the elevated helper E-B, the elevated agent E-C). Every result is a line `M5 <test> [<carrier>]:
PASS|FAIL <detail>` in the driver's log; the raw bytes of a failure are in `<out>/m5/`.

A carrier label is a carrier name (`m5_carriers.CARRIERS`), optionally followed by `/<where>`: `C1`, `C2/EC` (C2 through
the elevated agent), `EB`. `ssh_lib.init` and `init()` come first.
"""

import base64
import itertools
import re
import subprocess
import time
import traceback
from collections.abc import Callable
from pathlib import Path

import m5_carriers as cr
import paramiko_lib
import ssh_lib
from ssh_lib import say

RAW: Path
TOTALS: dict[str, list[int]] = {}  # carrier label -> [passed, failed]
_ids = itertools.count(1)
_took = [0.0]
_wedges = [0]


def init() -> None:
    global RAW
    RAW = ssh_lib.out / "m5"
    RAW.mkdir(exist_ok=True)


def mask(status: int) -> int:
    return status & 0xFFFFFFFF


def show(data: bytes, limit: int = 120) -> str:
    return repr(data[:limit]) + (f"... ({len(data)} bytes)" if len(data) > limit else "")


def verdict(test: str, carrier: str, ok: bool, detail: str, out: bytes = b"", err: bytes = b"") -> bool:
    totals = TOTALS.setdefault(carrier, [0, 0])
    totals[0 if ok else 1] += 1
    say(f"M5 {test} [{carrier}]: {'PASS' if ok else 'FAIL'} {detail} took={_took[0]:.1f}s")
    if not ok:
        stem = re.sub(r"[^A-Za-z0-9._-]", "_", f"{test}-{carrier}")[:80] + f"-{next(_ids)}"  # a test name may hold any character
        (RAW / f"{stem}.out").write_bytes(out)
        (RAW / f"{stem}.err").write_bytes(err)
    return ok


def note_wedge() -> None:
    """sshd stopped answering: the host's view of the forwarded port (state counts), for the first few times."""
    _wedges[0] += 1
    if _wedges[0] > 3:
        return
    text = subprocess.run(["netstat", "-ano", "-p", "tcp"], capture_output=True, text=True).stdout
    states: dict[str, int] = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) >= 4 and (parts[1].endswith(f":{ssh_lib.port}") or parts[2].endswith(f":{ssh_lib.port}")):
            states[parts[3]] = states.get(parts[3], 0) + 1
    say(f"host: TCP connections on the forwarded port {ssh_lib.port} by state: {states}")


def run_script(script: str, stdin: bytes = b"") -> tuple[int, bytes, bytes]:
    """exec_raw, with a command that did not end reported as status -2 and the reason on stderr. A guest that does not
    send its banner raises Wedged: that is not a result of the command."""
    began = time.monotonic()
    try:
        return paramiko_lib.exec_raw(script, stdin)
    except paramiko_lib.Wedged:
        note_wedge()
        raise
    except Exception as error:  # the test records it as a failure and the battery goes on
        return -2, b"", f"{type(error).__name__}: {error}".encode()
    finally:
        _took[0] = time.monotonic() - began


def run(carrier: str, argv: list[str], cwd: str | None = None, stdin: bytes = b"") -> tuple[int, bytes, bytes]:
    return run_script(cr.CARRIERS[carrier.split("/")[0]](argv, cwd), stdin)


def section(name: str, body: Callable[[], None]) -> None:
    """One group of tests. A section does not start when sshd does not answer, and a section's failure does not end the others."""
    say(f"== {name}")
    try:
        if not paramiko_lib.reachable():
            note_wedge()
            say(f"{name} SKIPPED: sshd does not send its banner")
            return
        body()
    except paramiko_lib.Wedged as error:
        say(f"{name} ENDED: sshd stopped answering during the section: {error}")
    except BaseException:
        say(f"{name} RAISED: {traceback.format_exc()}")


def summary() -> None:
    for carrier, (passed, failed) in sorted(TOTALS.items()):
        say(f"M5 SUMMARY [{carrier}]: {passed} passed, {failed} failed")


HOSTILE = [
    "", "a\nb", "it's", '"quoted"', "$x", "`id`", "%", "^", "&", ";", "\u00e9 \u0416 \u4e16 \U0001f600", "-x", "a=b",
    "  lead and trail  ", "\\n", "\u2018", "\u2019", "\u201a", "\u201b", "a\u2018b\u2019c\u201ad\u201be", "\u201cx\u201d",
    "trailing\\", "trailing\\\\", 'say "hi" \\', 'a\\"b', "C:\\Program Files\\x\\", "\\\\server\\share\\", "a b c", "tab\there",
    "|", "<", ">", "(", ")", "!", "*", "?", "[ab]", "{a,b}", "~", "#", "@", "--%", "-", "--", "/?", "/c", "x" * 5000,
]  # fmt: skip
# What the elevated carriers repeat (each command there costs seconds).
HOSTILE_SHORT = ["", "a\nb", "it's", '"quoted"', "$x", "`id`", "\u00e9 \u0416 \u4e16 \U0001f600", "\u2018", "trailing\\", "--%", "x" * 5000]
WORK = r"C:\ProgramData\devvm\m5\work"
PROBE = ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", r"C:\ProgramData\devvm\probe.ps1"]


def megabytes(count: int) -> bytes:
    return b"".join(cr.pattern(k) for k in range(count))


# The tests ============================================================================================================


def bytes_and_streams(carriers: tuple[str, ...]) -> None:
    """All 256 byte values, CRLF, 4 MiB on both streams, and stdin of 256 bytes. The large stdin tests (`stdin_large`)
    run last: they are the ones that wedged sshd."""
    all256 = bytes(range(256))
    for carrier in carriers:
        status, out, err = run(carrier, ["emit", "all256"])
        verdict("all256", carrier, mask(status) == 0 and out == all256 and err == all256[::-1],
                f"status={status} stdout_ok={out == all256} stderr_ok={err == all256[::-1]} out={show(out)} err={show(err)}", out, err)  # fmt: skip
        status, out, err = run(carrier, ["emit", "crlf"])
        want_out, want_err = b"a\r\nb\nc\r\n\r\nlast", b"e1\r\ne2\n"
        verdict("crlf-and-no-trailing-newline", carrier, out == want_out and err == want_err,
                f"status={status} out={show(out)} err={show(err)}", out, err)  # fmt: skip
        status, out, err = run(carrier, ["emit", "mib", "4"])
        want_out, want_err = megabytes(4), b"".join(cr.pattern(k, stream="err") for k in range(4))
        verdict("4mib-both-streams", carrier, out == want_out and err == want_err,
                f"status={status} stdout={len(out)}/{len(want_out)} ok={out == want_out} stderr={len(err)}/{len(want_err)} ok={err == want_err}", out[:4096], err[:4096])  # fmt: skip
        status, out, err = run(carrier, ["emit", "cat"], stdin=all256)
        verdict("stdin-all256", carrier, mask(status) == 0 and out == all256,
                f"status={status} stdout={len(out)}/{len(all256)} ok={out == all256} err={show(err)}", out[:4096], err)  # fmt: skip


def stdin_large(carriers: tuple[str, ...], mib: int) -> None:
    """`mib` MiB through stdin and back out of `emit cat`."""
    data = megabytes(mib)
    for carrier in carriers:
        status, out, err = run(carrier, ["emit", "cat"], stdin=data)
        verdict(f"stdin-{mib}mib", carrier, mask(status) == 0 and out == data,
                f"status={status} stdout={len(out)}/{len(data)} ok={out == data} err={show(err)}", out[:4096], err)  # fmt: skip


def exit_codes(carriers: tuple[str, ...], codes: tuple[int, ...] = (0, 7, 125, 255, 3221225477, -1, 2147483648, 256)) -> None:
    for carrier in carriers:
        for code in codes:
            status, out, err = run(carrier, ["emit", "exit", str(code)])
            verdict(f"exit-{code}", carrier, mask(status) == mask(code), f"status={status} want={mask(code)} err={show(err)}", out, err)


def argv_checks(carriers: tuple[str, ...], hostile: list[str] = HOSTILE) -> None:
    for carrier in carriers:
        for arg in hostile:
            status, out, err = run(carrier, ["argv", arg])
            seen = cr.parse_argv(out)
            verdict(f"argv-{arg[:30]!r}", carrier, mask(status) == 0 and seen == [arg],
                    f"status={status} got={seen!r:.200} err={show(err)}", out, err)  # fmt: skip
        together = hostile[:40]
        status, out, err = run(carrier, ["argv", *together])
        seen = cr.parse_argv(out)
        verdict("argv-all-together", carrier, seen == together, f"status={status} count={None if seen is None else len(seen)}", out, err)


def names_and_cwd(carriers: tuple[str, ...], full: bool = True) -> None:
    for carrier in carriers:
        if full:
            for name in ("-x", "a=b"):
                status, out, err = run(carrier, [name, "ok"])
                verdict(f"program-called-{name}", carrier, mask(status) == 0 and cr.parse_argv(out) == ["ok"],
                        f"status={status} out={show(out)} err={show(err)}", out, err)  # fmt: skip
            status, out, err = run(carrier, ["no-such-program-xyz"])
            verdict("missing-program", carrier, mask(status) == 127 and b"no-such-program-xyz" in err, f"status={status} err={show(err)}", out, err)
            status, out, err = run(carrier, [cr.BIN + r"\argv.exe", "full path"])
            verdict("program-by-full-path", carrier, cr.parse_argv(out) == ["full path"], f"status={status} out={show(out)} err={show(err)}", out, err)
        for sub in ("plain", "dir with space", "[brackets]", "\u00e9\u4e16", "it's") if full else ("dir with space", "\u00e9\u4e16"):
            want = WORK + "\\" + sub
            status, out, err = run(carrier, ["emit", "cwd"], cwd=want)
            got = cr.parse_cwd(out)
            verdict(f"cwd-{sub}", carrier, got == want, f"status={status} got={got!r} err={show(err)}", out, err)  # fmt: skip
        if full:
            status, out, err = run(carrier, ["emit", "cwd"], cwd=WORK + r"\does-not-exist")
            verdict("cwd-missing", carrier, mask(status) != 0, f"status={status} err={show(err)}", out, err)


CMD_ARGS = ["a b", 'q"uote', "a&b", "100%", "^caret", "a|b", "(paren)", "trailing\\", "", "plain", 'two "q" here', "a;b,c=d", "!bang!"]
SCRIPT = cr.BIN + r"\showargs.cmd"


def cmd_targets() -> None:
    for carrier in ("C1", "C1win", "C2", "C2legacy"):
        for arg in CMD_ARGS:
            status, out, err = run(carrier, [SCRIPT, arg])
            seen = cr.parse_argv(out)
            verdict(f"cmd-file-{arg!r}", carrier, seen == [arg], f"status={status} got={seen!r:.200} err={show(err)}", out, err)  # fmt: skip
        argv_exe = f'"{cr.BIN}\\argv.exe"'
        scripts = [
            ("echo", "echo hello", b"hello\r\n", 0),
            ("two-commands", "echo a & echo b", b"a\r\nb\r\n", 0),
            ("exit-3", "exit /b 3", b"", 3),
            ("quoted-program", f'{argv_exe} "a b" "c&d" "q""x"', None, 0),
            ("and-exit-4", 'echo "x" && exit /b 4', b'"x" \r\n', 4),
        ]
        for label, script, want_out, want_status in scripts:
            status, out, err = run(carrier, ["cmd.exe", "/d", "/c", script])
            if want_out is None:
                seen = cr.parse_argv(out)
                ok = seen == ["a b", "c&d", 'q"x'] and mask(status) == want_status
                detail = f"status={status} got={seen!r:.200}"
            else:
                ok = out.replace(b" \r\n", b"\r\n") == want_out.replace(b" \r\n", b"\r\n") and mask(status) == want_status
                detail = f"status={status} out={show(out)}"
            verdict(f"cmd-c-{label}", carrier, ok, f"{detail} err={show(err)}", out, err)


NESTED_TEXT = 'it\'s "q" $x `t \u00e9 \u4e16 \U0001f600 \u2018\u2019\u201a\u201b \\'
UTF8_OUT = "[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); "


def nested_pwsh(carriers: tuple[str, ...], full: bool = True) -> None:
    hex_script = "[BitConverter]::ToString([Text.Encoding]::UTF8.GetBytes(" + cr.q(NESTED_TEXT) + "))"
    want_hex = "-".join(f"{b:02X}" for b in NESTED_TEXT.encode("utf-8"))
    multi = "$t = 0\nforeach ($i in 1..3) {\n    $t += $i\n}\n\"sum=$t\"\n"
    encoded = base64.b64encode(hex_script.encode("utf-16-le")).decode("ascii")
    text = "\u00e9\u4e16".encode()
    cases = [
        ("-Command-hostile-literal", ["-Command", hex_script], want_hex.encode() + b"\r\n", 0),
        ("-EncodedCommand-hostile-literal", ["-EncodedCommand", encoded], want_hex.encode() + b"\r\n", 0),
        ("-Command-exit-7", ["-Command", "exit 7"], b"", 7),
        # What pwsh does with text: the console code page by default, UTF-8 only when the script says so.
        ("-Command-writes-non-ascii", ["-Command", "[Console]::Out.Write('\u00e9\u4e16')"], text, 0),
        ("-Command-writes-non-ascii-after-OutputEncoding-UTF8", ["-Command", UTF8_OUT + "[Console]::Out.Write('\u00e9\u4e16')"], text, 0),
        ("-Command-Write-Output-non-ascii", ["-Command", "Write-Output '\u00e9\u4e16'"], text + b"\r\n", 0),
        ("-Command-Write-Output-non-ascii-after-OutputEncoding-UTF8", ["-Command", UTF8_OUT + "Write-Output '\u00e9\u4e16'"], text + b"\r\n", 0),
    ]
    if full:
        cases.insert(2, ("-Command-multiline-double-quotes", ["-Command", multi], b"sum=6\r\n", 0))
    for carrier in carriers:
        for label, tail, want_out, want_status in cases:
            status, out, err = run(carrier, [cr.PWSH, "-NoProfile", "-NonInteractive", *tail])
            same = out.replace(b"\r\n", b"\n") == want_out.replace(b"\r\n", b"\n")
            verdict(f"nested-{label}", carrier, same and mask(status) == want_status,
                    f"status={status} out={show(out)} want={show(want_out)} err={show(err)}", out, err)  # fmt: skip


def longest_command(carriers: tuple[str, ...], sizes: tuple[int, ...]) -> None:
    for carrier in carriers:
        base = cr.CARRIERS[carrier.split("/")[0]](["argv", "padded"], None)
        for size in sizes:
            padded = "<#" + "x" * size + "#>\n" + base
            status, out, err = run_script(padded)
            verdict(f"command-of-{len(padded)}-characters", carrier, cr.parse_argv(out) == ["padded"],
                    f"status={status} out={show(out, 60)} err={show(err, 200)}", out, err)  # fmt: skip


def facts(carriers: tuple[str, ...], session: str, integrity: str) -> None:
    """What the guest's probe reports from inside the carrier (the elevated carriers must say High)."""
    for carrier in carriers:
        status, out, err = run(carrier, PROBE)
        seen = ssh_lib.facts(out)
        keys = ("session", "integrity", "elevated", "hkcu", "dpapi", "userprofile", "agents")
        verdict("facts", carrier, (seen.get("session"), seen.get("integrity")) == (session, integrity),
                f"status={status} " + " ".join(f"{key}={seen.get(key)}" for key in keys) + f" err={show(err)}", out, err)  # fmt: skip
