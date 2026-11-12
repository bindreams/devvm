"""Spike only (never merged): a raw SSH client for the measurements that must see the bytes the guest sends (M5, M10):
`paramiko` has no console, no CRLF translation and no command-line length limit, like the Rust client devvm will use.
Needs `ssh_lib.init` first (key directory and port).
"""

import re
import threading

import paramiko

import ssh_lib

# The failure bound for one command (an external event that may never come: a wrapper that waits for a stdin EOF it
# never gets). The error names it, and the battery goes on with the next test.
COMMAND_BOUND = 300


def connect(known_hosts: str = "known_hosts") -> paramiko.SSHClient:
    client = paramiko.SSHClient()
    client.load_host_keys(str(ssh_lib.keys / known_hosts))
    client.set_missing_host_key_policy(paramiko.RejectPolicy())
    client.connect(
        "127.0.0.1", port=int(ssh_lib.port), username="user",
        pkey=paramiko.Ed25519Key.from_private_key_file(str(ssh_lib.keys / "client")),
        allow_agent=False, look_for_keys=False,
    )  # fmt: skip
    return client


def exec_raw(command: str, stdin: bytes = b"", pty: bool = False) -> tuple[int, bytes, bytes]:
    """Runs `command` as an SSH exec request. Returns the exit status as sent (unsigned 32 bits), stdout, stderr."""
    client = connect()
    try:
        chan = client.get_transport().open_session()
        if pty:
            chan.get_pty(term="xterm-256color", width=120, height=30)
        chan.exec_command(command)
        out = bytearray()
        err = bytearray()

        def pump(read, sink: bytearray) -> None:
            while True:
                data = read(65536)
                if not data:
                    return
                sink += data

        def feed() -> None:
            try:
                if stdin:
                    chan.sendall(stdin)
                chan.shutdown_write()
            except (OSError, EOFError):  # the command ended, or the channel was closed on the bound below
                pass

        threads = [threading.Thread(target=pump, args=(chan.recv, out)), threading.Thread(target=pump, args=(chan.recv_stderr, err)),
                   threading.Thread(target=feed)]  # fmt: skip
        for thread in threads:
            thread.start()
        if not chan.status_event.wait(COMMAND_BOUND):
            chan.close()
            raise TimeoutError(f"no exit status {COMMAND_BOUND} s after the command started")
        status = chan.recv_exit_status()
        for thread in threads:
            thread.join()
        return status, bytes(out), bytes(err)
    finally:
        client.close()


class Screen:
    """A shell channel's output, collected by a thread; `wait` re-checks the pattern on every arrival."""

    DSR = b"\x1b[6n"  # ConPTY asks the terminal for the cursor position and waits for the answer

    def __init__(self, chan: paramiko.Channel) -> None:
        self.chan = chan
        self.data = bytearray()
        self.cond = threading.Condition()
        self.eof = False
        threading.Thread(target=self.read, daemon=True).start()

    def read(self) -> None:
        while True:
            data = self.chan.recv(4096)
            if self.DSR in data:
                self.chan.send(b"\x1b[1;1R")
            with self.cond:
                if not data:
                    self.eof = True
                    self.cond.notify_all()
                    return
                self.data += data
                self.cond.notify_all()

    def text(self) -> str:
        with self.cond:
            return self.data.decode("utf-8", errors="replace")

    def wait(self, pattern: str, after: int = 0) -> re.Match | None:
        """The first match of `pattern` in the text after offset `after`, or None when the channel ended first."""
        compiled = re.compile(pattern)
        with self.cond:
            while True:
                text = self.data.decode("utf-8", errors="replace")
                found = compiled.search(text, after)
                if found:
                    return found
                if self.eof:
                    return None
                self.cond.wait()
