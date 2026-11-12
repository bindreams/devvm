"""Spike only (never merged): the pty smoke of M5 (plan review finding 11): a login shell, the exit status, a window-size
change, Ctrl-C ending a running command, and an exec request with a pty. The shell's own output is the evidence; the raw
transcript is `<out>/pty-shell.raw`. Each marker is typed as a concatenation so that the echo of the typed line cannot match.
"""

import m5_carriers as cr
import paramiko_lib
import ssh_lib
from paramiko_lib import Screen
from ssh_lib import say


def marker(tag: str, expression: str) -> str:
    """A PowerShell line that prints `<tag>:<value>` where the typed text does not contain `<tag>:`."""
    return f"Write-Output ('{tag[0]}'+'{tag[1:]}:'+({expression}))\r"


def ask(screen: Screen, chan, tag: str, expression: str) -> str | None:
    after = len(screen.text())
    chan.send(marker(tag, expression).encode())
    found = screen.wait(rf"{tag}:([^\r\n\x1b]*)", after)
    return found.group(1).strip() if found else None


def shell() -> None:
    client = paramiko_lib.connect()
    screen = None
    try:
        chan = client.get_transport().open_session()
        chan.get_pty(term="xterm-256color", width=120, height=30)
        chan.invoke_shell()
        screen = Screen(chan)
        say(f"pty shell: pid {ask(screen, chan, 'PIDX', '$PID')}")
        say(f"pty shell: session {ask(screen, chan, 'SESX', '[Diagnostics.Process]::GetCurrentProcess().SessionId')}")
        host = "$Host.Name + '/' + $PSVersionTable.PSVersion"
        say(f"pty shell: host {ask(screen, chan, 'HOSTX', host)}")
        size = "$Host.UI.RawUI.WindowSize.Width.ToString() + 'x' + $Host.UI.RawUI.WindowSize.Height.ToString()"
        say(f"pty shell: size at start (asked 120x30): {ask(screen, chan, 'WINA', size)}")
        chan.resize_pty(width=90, height=25)
        say(f"pty shell: size after resize_pty 90x25: {ask(screen, chan, 'WINB', size)}")

        # Ctrl-C ends the running helper and the shell is still there afterwards.
        after = len(screen.text())
        chan.send(f"& '{cr.BIN}\\emit.exe' wait\r".encode())
        started = screen.wait("STARTED", after)
        say(f"pty shell: helper started: {bool(started)}")
        chan.send(b"\x03")
        interrupted = screen.wait("INTERRUPTED", after)
        say(f"pty shell: Ctrl-C reached the helper: {bool(interrupted)} (waited {paramiko_lib.SCREEN_BOUND} s at most)")
        if not interrupted:
            say(f"pty shell: screen after the Ctrl-C byte: {screen.text()[after:][-400:]!r}")
            return
        say(f"pty shell: shell alive after Ctrl-C: {ask(screen, chan, 'ALIVE', '1+1')}")

        chan.send(b"exit 7\r")
        ended = chan.status_event.wait(paramiko_lib.SCREEN_BOUND)
        say(f"pty shell: `exit 7` -> exit status {chan.recv_exit_status() if ended else 'none within the bound'}")
    finally:
        if screen is not None:
            (ssh_lib.out / "pty-shell.raw").write_bytes(bytes(screen.data))
        client.close()


def exec_with_pty() -> None:
    for label, argv in (("exit-7", ["emit", "exit", "7"]), ("crlf", ["emit", "crlf"]), ("all256", ["emit", "all256"])):
        status, out, err = paramiko_lib.exec_raw(cr.c2(argv), pty=True)
        say(f"pty exec {label}: status={status} stdout={out[:80]!r} ({len(out)} bytes) stderr={err[:80]!r}")


def measure() -> None:
    say("pty: a login shell")
    shell()
    say("pty: exec requests with a pty")
    exec_with_pty()
