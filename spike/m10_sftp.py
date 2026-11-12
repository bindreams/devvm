"""Spike only (never merged): M10, the guest's `sftp-server` through the broker, measured with a real SFTP client
(paramiko). The guest prepared `C:\\Users\\user\\m10\\` (`target.txt`, `subdir\\`, a file symlink and a directory
symlink). Every line is `M10 <what>: ok <result>` or `M10 <what>: <exception>: <message>` in `<out>/driver.log`.
"""

import re

import paramiko

import m5_carriers as cr
import paramiko_lib
from ssh_lib import say

HOME = "C:/Users/user/m10"
# The same file in every spelling a recipe or a user could type.
FORMS = [
    r"C:\Users\user\m10\forms.txt", "C:/Users/user/m10/forms.txt", "/C:/Users/user/m10/forms.txt",
    "c:/users/user/m10/forms.txt", r"\Users\user\m10\forms.txt", "/Users/user/m10/forms.txt", "m10/forms.txt",
    r"m10\forms.txt", "~/m10/forms.txt", "./m10/forms.txt", "/c/Users/user/m10/forms.txt",
]  # fmt: skip


def plain(data: bytes) -> str:
    """Text without the terminal colour sequences PowerShell puts in its error messages."""
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", data.decode("utf-8", errors="replace"))


def attempt(what: str, action):
    try:
        result = action()
        say(f"M10 {what}: ok {result!r}")
        return result
    except Exception as error:  # the message is the measurement
        say(f"M10 {what}: {type(error).__name__}: {error}")
        return None


def forms(sftp: paramiko.SFTPClient) -> None:
    for name in (".", "", "~", "/", "C:/", "/C:/", "/C:/Users/user", r"C:\Users\user"):
        attempt(f"realpath({name!r})", lambda name=name: sftp.normalize(name))
    for form in FORMS:
        def write(form=form):
            with sftp.open(form, "wb") as handle:
                handle.write(form.encode())
            return sftp.stat(form).st_size

        wrote = attempt(f"write via {form!r}", write)
        if wrote is not None:
            attempt(f"read it back through the slash form after writing via {form!r}", lambda: sftp.open(f"{HOME}/forms.txt").read())
    for form in (r"C:\Users\user\m10", "C:/Users/user/m10", "/C:/Users/user/m10", "m10", "m10/", "~/m10"):
        attempt(f"listdir({form!r})", lambda form=form: sorted(sftp.listdir(form)))
    for name in (f"{HOME}/forms.txt",):
        attempt(f"stat attrs of {name!r}", lambda: (sftp.stat(name).st_mode, sftp.stat(name).st_size))


def links(sftp: paramiko.SFTPClient) -> None:
    for name in ("link-file", "link-dir"):
        attempt(f"lstat({name})", lambda name=name: oct(sftp.lstat(f"{HOME}/{name}").st_mode))
        attempt(f"stat({name})", lambda name=name: oct(sftp.stat(f"{HOME}/{name}").st_mode))
        attempt(f"readlink({name})", lambda name=name: sftp.readlink(f"{HOME}/{name}"))
    attempt("listdir_attr shows the links", lambda: [(a.filename, oct(a.st_mode)) for a in sftp.listdir_attr(HOME)])
    # The protocol's own argument order is the reverse of what the name suggests; both are tried.
    attempt("symlink(target.txt, new-link-a)", lambda: sftp.symlink("target.txt", f"{HOME}/new-link-a"))
    attempt("readlink(new-link-a)", lambda: sftp.readlink(f"{HOME}/new-link-a"))
    attempt("symlink(new-link-b, target.txt)", lambda: sftp.symlink(f"{HOME}/new-link-b", "target.txt"))
    attempt("readlink(new-link-b)", lambda: sftp.readlink(f"{HOME}/new-link-b"))


def times(sftp: paramiko.SFTPClient) -> None:
    path = f"{HOME}/times.txt"
    attempt("create times.txt", lambda: sftp.open(path, "wb").close())
    for seconds in (0, 2**31 - 1, 2**31, 4102444800, 2**32 - 1, 2**32 + 5):
        def set_and_read(seconds=seconds):
            sftp.utime(path, (seconds, seconds))
            attrs = sftp.stat(path)
            return (attrs.st_atime, attrs.st_mtime)

        attempt(f"utime {seconds}", set_and_read)


def temp_files(sftp: paramiko.SFTPClient) -> None:
    final = f"{HOME}/upload.bin"
    temp = f"{HOME}/upload.bin.tmp"
    handle = sftp.open(temp, "wb")
    attempt("write 10 bytes, handle open", lambda: handle.write(b"x" * 10) or handle.flush())
    attempt("remove the temp while its handle is open", lambda: sftp.remove(temp))
    attempt("temp exists afterwards", lambda: sftp.stat(temp).st_size)
    attempt("rename the temp while its handle is open", lambda: sftp.rename(temp, final))
    attempt("posix_rename the temp while its handle is open", lambda: sftp.posix_rename(temp, final))
    attempt("close the handle", handle.close)
    for name in (temp, final):
        attempt(f"remove {name} after close", lambda name=name: sftp.remove(name))
    attempt("temp or final left behind", lambda: sorted(n for n in sftp.listdir(HOME) if n.startswith("upload")))
    # An overwrite by rename, as an upload that replaces a file does.
    attempt("write a", lambda: sftp.open(f"{HOME}/a.txt", "wb").write(b"a"))
    attempt("write b", lambda: sftp.open(f"{HOME}/b.txt", "wb").write(b"b"))
    attempt("rename b over a", lambda: sftp.rename(f"{HOME}/b.txt", f"{HOME}/a.txt"))
    attempt("posix_rename b over a", lambda: (sftp.open(f"{HOME}/b.txt", "wb").write(b"b"), sftp.posix_rename(f"{HOME}/b.txt", f"{HOME}/a.txt")))


def bytes_round_trip(sftp: paramiko.SFTPClient) -> None:
    data = bytes(range(256)) * 4096 + cr.pattern(1)  # 2 MiB with CRLF-looking runs
    path = f"{HOME}/bytes.bin"

    def trip():
        with sftp.open(path, "wb") as handle:
            handle.write(data)
        with sftp.open(path, "rb") as handle:
            return handle.read() == data and sftp.stat(path).st_size == len(data)

    attempt("a 2 MiB binary file round trip", trip)


def process_tree() -> None:
    """While the SFTP session is open, what `sftp-server.exe` is, where it runs and who started it."""
    script = (
        "Get-CimInstance Win32_Process -Filter \"Name='sftp-server.exe'\" | ForEach-Object { "
        "$parent = Get-CimInstance Win32_Process -Filter \"ProcessId=$($_.ParentProcessId)\"; "
        "$owner = Invoke-CimMethod -InputObject $_ -MethodName GetOwner; "
        "\"pid=$($_.ProcessId) session=$($_.SessionId) owner=$($owner.Domain)\\$($owner.User) cmd=$($_.CommandLine) "
        "parent=$($parent.Name):$($parent.CommandLine)\" }"
    )
    status, out, err = paramiko_lib.exec_raw(f"powershell.exe -NoProfile -Command {cr.q(script)}")
    say(f"M10 sftp-server processes (exit {status}): {out.decode('utf-8', errors='replace').strip()} {err.decode('utf-8', errors='replace').strip()[:300]}")


SFTP_INIT = bytes([0, 0, 0, 5, 1, 0, 0, 0, 3])  # length 5, SSH_FXP_INIT, version 3


def diagnose(client: paramiko.SSHClient) -> None:
    """The subsystem did not start: what the guest's side did. Three looks: the subsystem channel itself, where
    `sftp-server.exe` is for the shell the broker starts, and the same program run as a command with an INIT packet."""
    chan = client.get_transport().open_session()
    chan.settimeout(paramiko_lib.SCREEN_BOUND)  # the subsystem may stay silent: the bound is the failure
    chan.invoke_subsystem("sftp")
    chan.sendall(SFTP_INIT)
    got = bytearray()
    try:
        while data := chan.recv(4096):
            got += data
    except TimeoutError:
        say(f"M10 diag: subsystem channel silent for {paramiko_lib.SCREEN_BOUND} s after INIT")
    errs = bytearray()
    while chan.recv_stderr_ready():
        errs += chan.recv_stderr(4096)
    ended = chan.status_event.wait(paramiko_lib.SCREEN_BOUND)
    say(f"M10 diag: subsystem reply {bytes(got)!r}; stderr {bytes(errs)!r}; exit status {chan.recv_exit_status() if ended else 'none'}")
    chan.close()

    look = (
        "Get-Command sftp-server.exe -ErrorAction SilentlyContinue | ForEach-Object Source; "
        "'path=' + $env:PATH; "
        r"'in-openssh-dir=' + (Test-Path 'C:\Program Files\OpenSSH\sftp-server.exe'); "
        r"Get-Content 'C:\ProgramData\ssh\sshd_config' | Select-String 'Subsystem|ForceCommand'"
    )
    status, out, err = paramiko_lib.exec_raw(look)
    say(f"M10 diag: lookup (exit {status}): {out.decode('utf-8', errors='replace').strip()!r} {err.decode('utf-8', errors='replace').strip()[:300]!r}")
    for label, carrier in (("C1", cr.c1), ("C2", cr.c2)):
        script = carrier([r"C:\Program Files\OpenSSH\sftp-server.exe"])
        status, out, err = paramiko_lib.exec_raw(script, SFTP_INIT)
        say(f"M10 diag: sftp-server.exe as a command through {label} (exit {status}): stdout {out!r} stderr {plain(err)[:1500]!r}")


def measure() -> None:
    client = paramiko_lib.connect()
    try:
        try:
            sftp = client.open_sftp()
        except Exception as error:  # the next lines say why
            say(f"M10 sftp subsystem did not start: {type(error).__name__}: {error}")
            diagnose(client)
            return
        # An operation that gets no answer is a failure line, not a stalled stage (the subsystem has never run before).
        sftp.get_channel().settimeout(paramiko_lib.SCREEN_BOUND)
        process_tree()
        forms(sftp)
        links(sftp)
        times(sftp)
        temp_files(sftp)
        bytes_round_trip(sftp)
    finally:
        client.close()
