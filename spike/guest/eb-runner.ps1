# Spike only (never merged). The elevated helper of M6 E-B: the scheduled task `devvm-eb` (user, Highest, no trigger)
# runs this once per request. The request directory is named by C:\ProgramData\devvm\eb\current.txt; it holds script.ps1
# and stdin. This runs `pwsh -File script.ps1` with stdin from that file and leaves stdout, stderr, exit (the status as
# a signed integer) and, last, done. Runs under pwsh 7 (ProcessStartInfo.ArgumentList).
$root = 'C:\ProgramData\devvm\eb'
$dir = (Get-Content -Raw -LiteralPath "$root\current.txt").Trim()
$exit = 125
try {
    $psi = [Diagnostics.ProcessStartInfo]::new('C:\Program Files\PowerShell\7\pwsh.exe')
    foreach ($arg in '-NoProfile', '-NonInteractive', '-File', "$dir\script.ps1") { $psi.ArgumentList.Add($arg) }
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $p = [Diagnostics.Process]::Start($psi)
    $in = [IO.File]::OpenRead("$dir\stdin")
    $out = [IO.File]::Create("$dir\stdout")
    $err = [IO.File]::Create("$dir\stderr")
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
    [IO.File]::WriteAllText("$dir\stderr", "eb-runner: $($_ | Out-String)")
}
foreach ($name in 'stdout', 'stderr') {
    if (-not [IO.File]::Exists("$dir\$name")) { [IO.File]::WriteAllBytes("$dir\$name", [byte[]]@()) }
}
Set-Content -LiteralPath "$dir\exit" -Value $exit
Set-Content -LiteralPath "$dir\done" -Value (Get-Date -Format o)
