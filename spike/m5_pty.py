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
        # Typed before the prompt is back, a line is lost (run 43260908167: the ALIVE line never reached the shell).
        prompt = screen.wait(r"PS [A-Za-z]:[^>\r\n]*> ", interrupted.end())
        say(f"pty shell: the prompt came back after Ctrl-C: {bool(prompt)}")
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
        (ssh_lib.out / f"pty-exec-{label}.raw").write_bytes(out)
        say(f"pty exec {label}: status={status} stdout={out[-80:]!r} ({len(out)} bytes) stderr={err[:80]!r}")
    # Did the command run at all? The pseudoconsole's output holds none of it (151 bytes, run 43278227165): a file the
    # command writes answers that, read back by an exec request without a pty.
    exec_marker("marker", pty=True)


def marker_path(label: str) -> str:
    return rf"C:\Users\Public\pty-exec-{label}.txt"


def marker_script(label: str) -> str:
    return (
        f"Set-Content -LiteralPath '{marker_path(label)}' -Value ('ran pid=' + $PID + ' session=' + "
        "[Diagnostics.Process]::GetCurrentProcess().SessionId + ' interactive=' + [Environment]::UserInteractive); "
        "Write-Output 'PTYEXEC-MARKER'"
    )


def read_marker(label: str) -> str:
    """The marker file of `label`, read by an exec request without a pty, or `marker file absent`."""
    path = marker_path(label)
    _, out, _ = paramiko_lib.exec_raw(f"if (Test-Path '{path}') {{ Get-Content -Raw '{path}' }} else {{ 'marker file absent' }}")
    return out.decode("utf-8", errors="replace").strip()


def exec_marker(label: str, command: str | None = None, pty: bool = True) -> None:
    """Runs `command` (default: `marker_script(label)`, which writes `marker_path(label)`) as an exec request, then reads
    the file back. The raw output is `pty-exec-<label>.raw`."""
    status, out, err = paramiko_lib.exec_raw(command or marker_script(label), pty=pty)
    (ssh_lib.out / f"pty-exec-{label}.raw").write_bytes(out)
    say(f"pty exec {label} (pty={pty}): status={status} stdout={out[-80:]!r} ({len(out)} bytes) stderr={err[:80]!r}")
    say(f"pty exec {label}: marker file (read without a pty): {read_marker(label)!r}")


def measure() -> None:
    say("pty: a login shell")
    shell()
    say("pty: exec requests with a pty")
    exec_with_pty()
