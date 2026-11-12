# /// script
# requires-python = ">=3.11"
# dependencies = ["paramiko"]
# ///
"""Spike only (never merged): the host side of stage `m5`. Runs beside the guest (run_qemu.py `--driver`) and measures
M5 (the wrapper battery through the broker and `pwsh -c`, carriers C1 and C2 of plan 6.5), M10 (the SFTP server) and a
pty smoke. Every result is a line `M5 <test> [<carrier>]: PASS|FAIL <detail>` in `<out>/driver.log`; the raw bytes of a
failure are in `<out>/m5/`. A section that raises is logged and the next one runs.

Usage: m5_driver.py <out dir> <key dir> <port>
"""

import sys
import time
import traceback
from collections.abc import Callable

import m5_carriers as cr
import m5_pty
import m10_sftp
import paramiko_lib
import ssh_lib
from ssh_lib import in_session, probe_until, say

ssh_lib.init(sys.argv[1], sys.argv[2], sys.argv[3], "driver.log")
sys.stderr.reconfigure(encoding="utf-8", errors="backslashreplace")
RAW = ssh_lib.out / "m5"
RAW.mkdir(exist_ok=True)
TOTALS: dict[str, list[int]] = {}  # carrier -> [passed, failed]


def mask(status: int) -> int:
    return status & 0xFFFFFFFF


def show(data: bytes, limit: int = 120) -> str:
    return repr(data[:limit]) + (f"... ({len(data)} bytes)" if len(data) > limit else "")


def verdict(test: str, carrier: str, ok: bool, detail: str, out: bytes = b"", err: bytes = b"") -> bool:
    totals = TOTALS.setdefault(carrier, [0, 0])
    totals[0 if ok else 1] += 1
    say(f"M5 {test} [{carrier}]: {'PASS' if ok else 'FAIL'} {detail}")
    if not ok:
        stem = f"{test}-{carrier}".replace("/", "_").replace("\\", "_").replace(":", "_")[:100]
        (RAW / f"{stem}.out").write_bytes(out)
        (RAW / f"{stem}.err").write_bytes(err)
    return ok


def run_script(script: str, stdin: bytes = b"") -> tuple[int, bytes, bytes]:
    """exec_raw, with a command that did not end reported as status -2 and the reason on stderr."""
    try:
        return paramiko_lib.exec_raw(script, stdin)
    except Exception as error:  # the test records it as a failure and the battery goes on
        return -2, b"", f"{type(error).__name__}: {error}".encode()


def run(carrier: str, argv: list[str], cwd: str | None = None, stdin: bytes = b"") -> tuple[int, bytes, bytes]:
    return run_script(cr.CARRIERS[carrier](argv, cwd), stdin)


def section(name: str, body: Callable[[], None]) -> None:
    say(f"== {name}")
    try:
        body()
    except BaseException:  # a section's failure must not end the others
        say(f"{name} RAISED: {traceback.format_exc()}")


ALL = ("C1", "C2")

HOSTILE = [
    "", "a\nb", "it's", '"quoted"', "$x", "`id`", "%", "^", "&", ";", "\u00e9 \u0416 \u4e16 \U0001f600", "-x", "a=b",
    "  lead and trail  ", "\\n", "\u2018", "\u2019", "\u201a", "\u201b", "a\u2018b\u2019c\u201ad\u201be", "\u201cx\u201d",
    "trailing\\", "trailing\\\\", 'say "hi" \\', 'a\\"b', "C:\\Program Files\\x\\", "\\\\server\\share\\", "a b c", "tab\there",
    "|", "<", ">", "(", ")", "!", "*", "?", "[ab]", "{a,b}", "~", "#", "@", "--%", "-", "--", "/?", "/c", "x" * 5000,
]  # fmt: skip


def bytes_and_streams() -> None:
    all256 = bytes(range(256))
    for carrier in ALL:
        status, out, err = run(carrier, ["emit", "all256"])
        verdict("all256", carrier, mask(status) == 0 and out == all256 and err == all256[::-1],
                f"status={status} stdout_ok={out == all256} stderr_ok={err == all256[::-1]} out={show(out)} err={show(err)}", out, err)  # fmt: skip
        status, out, err = run(carrier, ["emit", "crlf"])
        want_out, want_err = b"a\r\nb\nc\r\n\r\nlast", b"e1\r\ne2\n"
        verdict("crlf-and-no-trailing-newline", carrier, out == want_out and err == want_err,
                f"status={status} out={show(out)} err={show(err)}", out, err)  # fmt: skip
        mib = 4
        status, out, err = run(carrier, ["emit", "mib", str(mib)])
        want_out = b"".join(cr.pattern(k) for k in range(mib))
        want_err = b"".join(cr.pattern(k, stream="err") for k in range(mib))
        verdict(f"{mib}mib-both-streams", carrier, out == want_out and err == want_err,
                f"status={status} stdout={len(out)}/{len(want_out)} ok={out == want_out} stderr={len(err)}/{len(want_err)} ok={err == want_err}", out[:4096], err[:4096])  # fmt: skip
        for label, data in (("all256", all256), ("4mib", b"".join(cr.pattern(k) for k in range(mib)))):
            status, out, err = run(carrier, ["emit", "cat"], stdin=data)
            verdict(f"stdin-{label}", carrier, mask(status) == 0 and out == data,
                    f"status={status} stdout={len(out)}/{len(data)} ok={out == data} err={show(err)}", out[:4096], err)  # fmt: skip


def exit_codes() -> None:
    for carrier in ALL:
        for code in (0, 7, 125, 255, 3221225477, -1, 2147483648, 256):
            status, out, err = run(carrier, ["emit", "exit", str(code)])
            verdict(f"exit-{code}", carrier, mask(status) == mask(code), f"status={status} want={mask(code)} err={show(err)}", out, err)


def argv_checks() -> None:
    for carrier in ALL:
        for arg in HOSTILE:
            status, out, err = run(carrier, ["argv", arg])
            seen = cr.parse_argv(out)
            verdict(f"argv-{arg[:30]!r}", carrier, mask(status) == 0 and seen == [arg],
                    f"status={status} got={seen!r:.200} err={show(err)}", out, err)  # fmt: skip
        status, out, err = run(carrier, ["argv", *HOSTILE[:40]])
        seen = cr.parse_argv(out)
        verdict("argv-all-together", carrier, seen == HOSTILE[:40], f"status={status} count={None if seen is None else len(seen)}", out, err)  # fmt: skip


def names_and_cwd() -> None:
    work = r"C:\ProgramData\devvm\m5\work"
    for carrier in ALL:
        for name in ("-x", "a=b"):
            status, out, err = run(carrier, [name, "ok"])
            verdict(f"program-called-{name}", carrier, mask(status) == 0 and cr.parse_argv(out) == ["ok"],
                    f"status={status} out={show(out)} err={show(err)}", out, err)  # fmt: skip
        status, out, err = run(carrier, ["no-such-program-xyz"])
        verdict("missing-program", carrier, mask(status) == 127 and b"no-such-program-xyz" in err, f"status={status} err={show(err)}", out, err)
        status, out, err = run(carrier, [cr.BIN + r"\argv.exe", "full path"])
        verdict("program-by-full-path", carrier, cr.parse_argv(out) == ["full path"], f"status={status} out={show(out)} err={show(err)}", out, err)
        for sub in ("plain", "dir with space", "[brackets]", "\u00e9\u4e16", "it's"):
            want = work + "\\" + sub
            status, out, err = run(carrier, ["emit", "cwd"], cwd=want)
            got = cr.parse_cwd(out)
            verdict(f"cwd-{sub}", carrier, got == want, f"status={status} got={got!r} err={show(err)}", out, err)  # fmt: skip
        status, out, err = run(carrier, ["emit", "cwd"], cwd=work + r"\does-not-exist")
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


def nested_pwsh() -> None:
    import base64

    hex_script = "[BitConverter]::ToString([Text.Encoding]::UTF8.GetBytes(" + cr.q(NESTED_TEXT) + "))"
    want_hex = "-".join(f"{b:02X}" for b in NESTED_TEXT.encode("utf-8"))
    multi = "$t = 0\nforeach ($i in 1..3) {\n    $t += $i\n}\n\"sum=$t\"\n"
    encoded = base64.b64encode(hex_script.encode("utf-16-le")).decode("ascii")
    cases = [
        ("-Command-hostile-literal", ["-Command", hex_script], want_hex.encode() + b"\r\n", 0),
        ("-EncodedCommand-hostile-literal", ["-EncodedCommand", encoded], want_hex.encode() + b"\r\n", 0),
        ("-Command-multiline-double-quotes", ["-Command", multi], b"sum=6\r\n", 0),
        ("-Command-exit-7", ["-Command", "exit 7"], b"", 7),
        ("-Command-writes-non-ascii", ["-Command", "[Console]::Out.Write('\u00e9\u4e16')"], "\u00e9\u4e16".encode(), 0),
    ]
    for carrier in ALL:
        for label, tail, want_out, want_status in cases:
            status, out, err = run(carrier, [cr.PWSH, "-NoProfile", "-NonInteractive", *tail])
            same = out.replace(b"\r\n", b"\n") == want_out.replace(b"\r\n", b"\n")
            verdict(f"nested-{label}", carrier, same and mask(status) == want_status,
                    f"status={status} out={show(out)} want={show(want_out)} err={show(err)}", out, err)  # fmt: skip


def longest_command() -> None:
    sizes = [1_000, 8_000, 16_000, 24_000, 30_000, 31_000, 32_000, 32_500, 32_700, 32_760, 40_000, 100_000, 130_000]
    for carrier in ALL:
        base = cr.CARRIERS[carrier](["argv", "padded"], None)
        for size in sizes:
            padded = "<#" + "x" * size + "#>\n" + base
            status, out, err = run_script(padded)
            verdict(f"command-of-{len(padded)}-characters", carrier, cr.parse_argv(out) == ["padded"],
                    f"status={status} out={show(out, 60)} err={show(err, 200)}", out, err)  # fmt: skip


def summary() -> None:
    for carrier, (passed, failed) in sorted(TOTALS.items()):
        say(f"M5 SUMMARY [{carrier}]: {passed} passed, {failed} failed")


# The agent in session 1 first (the shim fails open into session 0 otherwise), then the sections.
say("waiting for SSH, then for the agent")
begin = time.time()
probe_until("window", in_session("1", "S-1-16-8192"))
say(f"agent session reached after {time.time() - begin:.1f}s")

section("M5 bytes and streams", bytes_and_streams)
section("M5 exit codes", exit_codes)
section("M5 argv", argv_checks)
section("M5 program names and cwd", names_and_cwd)
section("M5 cmd.exe and .cmd targets", cmd_targets)
section("M5 nested pwsh", nested_pwsh)
section("M5 longest command", longest_command)
section("M5 summary", summary)
section("M10 sftp", m10_sftp.measure)
section("pty smoke", m5_pty.measure)
say("done")
