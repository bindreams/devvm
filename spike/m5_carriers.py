"""Spike only (never merged): the PowerShell scripts M5 measures, as the Rust `wrap` for the Windows guest would build them
(plan 6.5): C1 calls the program from PowerShell, C2 starts it with System.Diagnostics.Process and copies raw streams.
Every value is a single-quoted literal. The scripts begin with the session check (exit 126 in session 0) and, for the
tests, put the helper directory first on PATH.
"""

from collections.abc import Callable

BIN = r"C:\ProgramData\devvm\m5\bin"
PWSH = r"C:\Program Files\PowerShell\7\pwsh.exe"
# PowerShell reads all four as a single quote inside a single-quoted string; doubling one is a literal one.
SINGLE_QUOTES = "'\u2018\u2019\u201a\u201b"

PREAMBLE = (
    "$ErrorActionPreference = 'Stop'\n"
    "if ([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) "
    "{ [Console]::Error.WriteLine('devvm: the command did not reach the user''s session'); exit 126 }\n"
    "$env:PATH = {bin} + ';' + $env:PATH\n"
)


def q(text: str) -> str:
    return "'" + "".join(ch * 2 if ch in SINGLE_QUOTES else ch for ch in text) + "'"


def head(argv0: str) -> str:
    return (
        PREAMBLE.replace("{bin}", q(BIN))
        + f"$app = Get-Command -CommandType Application -Name {q(argv0)} -ErrorAction SilentlyContinue | Select-Object -First 1\n"
        + f"if (-not $app) {{ [Console]::Error.WriteLine('devvm: ' + {q(argv0)} + ': command not found'); exit 127 }}\n"
    )


def c1(argv: list[str], cwd: str | None = None, passing: str = "Standard") -> str:
    """C1: `& program args`; `passing` is `$PSNativeCommandArgumentPassing`."""
    script = head(argv[0])
    if cwd:
        script += f"Set-Location -LiteralPath {q(cwd)}\n"
    script += f"$PSNativeCommandArgumentPassing = {q(passing)}\n"
    script += "& $app.Source " + " ".join(q(arg) for arg in argv[1:]) + "\n"
    return script + "exit $LASTEXITCODE\n"


C2_TAIL = """$p = [Diagnostics.Process]::Start($psi)
$stdout = [Console]::OpenStandardOutput()
$stderr = [Console]::OpenStandardError()
$ti = [Console]::OpenStandardInput().CopyToAsync($p.StandardInput.BaseStream)
$to = $p.StandardOutput.BaseStream.CopyToAsync($stdout)
$te = $p.StandardError.BaseStream.CopyToAsync($stderr)
$both = [Threading.Tasks.Task]::WhenAll($to, $te)
if ([Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]@($ti, $both)) -eq 0) { try { $p.StandardInput.Close() } catch { } }
$both.Wait()
$p.WaitForExit()
exit $p.ExitCode
"""


def c2(argv: list[str], cwd: str | None = None, raw_arguments: str | None = None) -> str:
    """C2: Process + ArgumentList (or the raw `Arguments` string), raw BaseStream copies."""
    script = head(argv[0])
    script += "$psi = [Diagnostics.ProcessStartInfo]::new($app.Source)\n"
    if cwd:
        script += f"$psi.WorkingDirectory = {q(cwd)}\n"
    script += "$psi.UseShellExecute = $false\n$psi.RedirectStandardInput = $true\n"
    script += "$psi.RedirectStandardOutput = $true\n$psi.RedirectStandardError = $true\n"
    if raw_arguments is None:
        script += "".join(f"$psi.ArgumentList.Add({q(arg)})\n" for arg in argv[1:])
    else:
        script += f"$psi.Arguments = {q(raw_arguments)}\n"
    return script + C2_TAIL


CMD_SPECIAL = set(' \t"&|<>^%()!,;=')


def legacy_quote(arg: str) -> str:
    """cmd.exe's own quoting: a quote inside quotes is doubled, the argument is wrapped when it needs it."""
    if arg and not any(ch in CMD_SPECIAL for ch in arg):
        return arg
    return '"' + arg.replace('"', '""') + '"'


def c2_legacy(argv: list[str], cwd: str | None = None) -> str:
    """C2 with a raw `Arguments` string for `cmd /c <script>` and `.cmd`/`.bat` targets, the list form otherwise."""
    name = argv[0].lower()
    if name in ("cmd", "cmd.exe") and len(argv) >= 3 and argv[-2].lower() == "/c":
        return c2(argv, cwd, " ".join(argv[1:-1]) + ' "' + argv[-1] + '"')
    if name.endswith((".cmd", ".bat")):
        return c2(argv, cwd, " ".join(legacy_quote(arg) for arg in argv[1:]))
    return c2(argv, cwd)


EB_ROOT = r"C:\ProgramData\devvm\eb"

# E-B (plan M6): the caller runs in the Medium session, hands the command to the elevated scheduled task `devvm-eb`
# (guest/stages/m5/eb-runner.ps1) through files and plays the result back. Stdin goes to a file first; stdout and
# stderr come back after the command ends (not live); the exit status is the helper's.
EB_CALLER = r"""$ErrorActionPreference = 'Stop'
$root = {root}
$dir = Join-Path $root ('req-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($dir)
[IO.File]::WriteAllText("$dir\script.ps1", {script}, [Text.UTF8Encoding]::new($false))
$in = [IO.File]::Create("$dir\stdin")
[Console]::OpenStandardInput().CopyTo($in)
$in.Close()
[IO.File]::WriteAllText("$root\current.txt", $dir)
Start-ScheduledTask -TaskName 'devvm-eb'
while (-not [IO.File]::Exists("$dir\done")) { Start-Sleep -Milliseconds 100 }
foreach ($pair in @(@('stdout', [Console]::OpenStandardOutput()), @('stderr', [Console]::OpenStandardError()))) {
    $file = [IO.File]::OpenRead("$dir\$($pair[0])")
    $file.CopyTo($pair[1])
    $file.Close()
}
exit ([int][IO.File]::ReadAllText("$dir\exit").Trim())
"""


def eb(argv: list[str], cwd: str | None = None) -> str:
    """E-B: the C2 script for `argv`, run by the elevated helper task, caller as above."""
    return EB_CALLER.replace("{root}", q(EB_ROOT)).replace("{script}", q(c2(argv, cwd)))


CARRIERS: dict[str, Callable[..., str]] = {
    "C1": c1,
    "C1win": lambda argv, cwd=None: c1(argv, cwd, "Windows"),
    "C2": c2,
    "C2legacy": c2_legacy,
    "EB": eb,
}

# The expected output of the `argv` helper: its argument count and each argument, as hex of the UTF-8 bytes.


def parse_argv(out: bytes) -> list[str] | None:
    args: dict[int, str] = {}
    try:
        for line in out.decode("ascii").splitlines():
            key, _, value = line.partition("=")
            if key.startswith("ARG") and key[3:].isdigit():
                args[int(key[3:])] = bytes.fromhex(value).decode("utf-8")
    except (UnicodeDecodeError, ValueError):
        return None
    return [args[i] for i in sorted(args)]


def parse_cwd(out: bytes) -> str | None:
    for line in out.decode("ascii", errors="replace").splitlines():
        if line.startswith("CWD="):
            try:
                return bytes.fromhex(line[4:]).decode("utf-8")
            except ValueError:
                return None
    return None


def pattern(chunk: int, size: int = 1 << 20, stream: str = "out") -> bytes:
    """The bytes `emit mib` writes for chunk number `chunk` (stderr's are the complement)."""
    data = bytes((i * 7 + chunk * 13 + (i >> 8)) & 0xFF for i in range(size))
    return data if stream == "out" else bytes(255 - b for b in data)

