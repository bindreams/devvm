# Spike only (never merged). Stage `m2`, boot task (SYSTEM), variant `system`: M2's second question. The same shape as the
# end of bootstrap.ps1: a guarded script fails, the `catch` sends DEVVM-BASE-FAILED=..., the volume cache is flushed and
# `shutdown /s` ends the guest with nobody asking it to. run_qemu.py `--expect-failed` passes when the marker is in the
# serial log after QEMU exited by itself.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
if ((Get-Content -Raw -LiteralPath (Join-Path $Root 'variant.txt')).Trim() -ne 'system') { return }

try {
    Send-Line 'DEVVM-PROGRESS=m2: the guarded script begins'
    throw 'm2: a deliberate failure'
} catch {
    Send-Line "DEVVM-BASE-FAILED=$($_ | Out-String)"
}
Write-VolumeCache -DriveLetter C -ErrorAction SilentlyContinue
shutdown.exe /s /t 0
