# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Spike only (never merged). The pass rule of the agent-window gate, applied to the guest's `windows.log` (winwatch.cs).

After the agent serves, no visible window of the agent's console may remain: no CASCADIA_HOSTING_WINDOW_CLASS window titled
with ssh-broker, no visible ConsoleWindowClass window, and no visible window owned by one of those (the pseudoconsole
window). Windows of WindowsTerminal.exe, OpenConsole.exe and conhost.exe that are not the agent's (the first-logon
PowerShell window) are listed, not failed.

The log keeps one block per change, from every boot, so the blocks that count are those of the last winwatch process (the
last `sample=1`): the block in effect when the agent began serving (the last one at or before that moment) and every
later one. Usage: winverdict.py <windows.log> <serve ticks>   (.NET ticks of the guest clock, as the driver takes them)
"""

import re
import sys
from datetime import datetime, timezone

TERMINAL_EXES = {"windowsterminal.exe", "openconsole.exe", "conhost.exe"}
HEADER = re.compile(r"^== (\d{4})-(\d\d)-(\d\d)T(\d\d):(\d\d):(\d\d)\.(\d{7})Z sample=(\d+) ")
WINDOW = re.compile(r"^(0x[0-9A-F]+) pid=(\d+) exe=(\S+) vis=(\d) .* owner=(0x[0-9A-F]+) class=(\S+) rect=\S+ title=\"(.*)\"$")
EPOCH_TICKS = 621355968000000000


def header_ticks(match: re.Match) -> int:
    year, month, day, hour, minute, second, fraction = (int(group) for group in match.groups()[:7])
    seconds = int(datetime(year, month, day, hour, minute, second, tzinfo=timezone.utc).timestamp())
    return EPOCH_TICKS + seconds * 10_000_000 + fraction


def blocks(text: str) -> list[tuple[int, int, list[str]]]:
    """(ticks, sample number, window lines) per block, in file order."""
    found: list[tuple[int, int, list[str]]] = []
    for line in text.splitlines():
        header = HEADER.match(line)
        if header:
            found.append((header_ticks(header), int(header.group(8)), []))
        elif found and line.startswith("0x"):
            found[-1][2].append(line)
    return found


def agent_windows(lines: list[str]) -> tuple[list[str], list[str]]:
    """(the agent's visible windows, other visible terminal windows) of one block."""
    parsed = []
    for line in lines:
        match = WINDOW.match(line)
        if match and match.group(4) == "1":
            parsed.append((match.group(1), match.group(3).lower(), match.group(5), match.group(6), match.group(7), line))
    agent = {
        hwnd
        for hwnd, _, _, cls, title, _ in parsed
        if cls == "ConsoleWindowClass" or "ssh-broker" in title.lower()
    }
    # A window owned by an agent window (the pseudoconsole window) is the agent's too.
    grown = True
    while grown:
        grown = False
        for hwnd, exe, owner, _, _, _ in parsed:
            if hwnd not in agent and owner in agent and exe in TERMINAL_EXES:
                agent.add(hwnd)
                grown = True
    mine = [line for hwnd, _, _, _, _, line in parsed if hwnd in agent]
    other = [line for hwnd, exe, _, _, _, line in parsed if hwnd not in agent and exe in TERMINAL_EXES]
    return mine, other


def verdict(text: str, serve_ticks: int) -> tuple[bool, str]:
    """(passed, a report). The window list must have been sampled at least once by the last winwatch process."""
    every = blocks(text)
    starts = [index for index, (_, sample, _) in enumerate(every) if sample == 1]
    if not starts:
        return False, "FAIL: no window list in the log"
    own = every[starts[-1] :]
    before = [block for block in own if block[0] <= serve_ticks]
    counting = ([before[-1]] if before else []) + [block for block in own if block[0] > serve_ticks]
    report = [f"blocks of the last winwatch process: {len(own)}; counting: {len(counting)} (in effect at serve, and later)"]
    passed = True
    for ticks, sample, lines in counting:
        mine, other = agent_windows(lines)
        report.append(f"block sample={sample} ticks={ticks}: agent windows={len(mine)}, other visible terminal windows={len(other)}")
        for line in mine:
            passed = False
            report.append(f"  AGENT WINDOW {line}")
        for line in other:
            report.append(f"  other {line}")
    report.append(f"last block (sample={own[-1][1]}): agent windows={len(agent_windows(own[-1][2])[0])}")
    report.insert(0, "PASS: no visible window of the agent's console" if passed else "FAIL: the agent's console has a visible window")
    return passed, "\n".join(report)


if __name__ == "__main__":
    ok, text = verdict(open(sys.argv[1], encoding="utf-8", errors="replace").read(), int(sys.argv[2]))
    print(text)
    sys.exit(0 if ok else 1)
