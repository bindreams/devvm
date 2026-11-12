# Spike only (never merged). The parallel elevated helper of M6 (task `devvm-eb2`, user, Highest, MultipleInstances
# Parallel). One start serves one request: the caller starts the task with `Run($dir)`, Task Scheduler passes the
# directory as `$(Arg0)`, so no file names "the current request". The directory holds script.ps1 and stdin. This writes
# `started` (UTC ticks and PID) first, then stdout, stderr, exit and, last, `done`. Runs under pwsh 7.
param([Parameter(Mandatory)][string]$Dir)
[IO.File]::WriteAllText("$Dir\started", "$([DateTime]::UtcNow.Ticks) $PID")
$exit = 125
try {
    $psi = [Diagnostics.ProcessStartInfo]::new('C:\Program Files\PowerShell\7\pwsh.exe')
    foreach ($arg in '-NoProfile', '-NonInteractive', '-File', "$Dir\script.ps1") { $psi.ArgumentList.Add($arg) }
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [Diagnostics.Process]::Start($psi)
    $in = [IO.File]::OpenRead("$Dir\stdin")
    $out = [IO.File]::Create("$Dir\stdout")
    $err = [IO.File]::Create("$Dir\stderr")
    $ti = $in.CopyToAsync($p.StandardInput.BaseStream)
    $to = $p.StandardOutput.BaseStream.CopyToAsync($out)
    $te = $p.StandardError.BaseStream.CopyToAsync($err)
    try { $ti.Wait() } catch { }  # the command may end without reading all of its stdin
    try { $p.StandardInput.Close() } catch { }
    $p.WaitForExit()
    [Threading.Tasks.Task]::WaitAll([Threading.Tasks.Task[]]@($to, $te))
    $in.Close(); $out.Close(); $err.Close()
    $exit = $p.ExitCode
} catch {
    [IO.File]::WriteAllText("$Dir\stderr", "eb-runner2: $($_ | Out-String)")
}
foreach ($name in 'stdout', 'stderr') {
    if (-not [IO.File]::Exists("$Dir\$name")) { [IO.File]::WriteAllBytes("$Dir\$name", [byte[]]@()) }
}
Set-Content -LiteralPath "$Dir\exit" -Value $exit
Set-Content -LiteralPath "$Dir\done" -Value ([DateTime]::UtcNow.Ticks)
