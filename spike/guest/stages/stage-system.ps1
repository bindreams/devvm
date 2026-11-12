# Spike only (never merged). Stage `boot1`, boot task (SYSTEM).
# 1. Records what a boot looks like before anyone is logged on.
# 2. The base's own lib.ps1 and hook.ps1 fail for the user's logon task (Global\ mutex, Get-Volume): replaces them with
#    the fixed copies from this disk and runs the user's task again, then relays what the task logged to COM1.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1

$boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
Send-Line "DEVVM-M15-BOOTTIME=$($boot.ToUniversalTime().ToString('o')) uptime-seconds=$([int]((Get-Date) - $boot).TotalSeconds)"
Send-Line "DEVVM-STAGE-WHO=$(whoami) session $(Get-SessionId) integrity $(Get-IntegritySid)"
Send-Probe 'M4-SESSIONS' { query user 2>&1; query session 2>&1 }
Send-Probe 'M4-TASK-BEFORE' {
    Get-ScheduledTaskInfo -TaskName 'devvm-stage-user' | Select-Object LastRunTime, LastTaskResult, NumberOfMissedRuns | ConvertTo-Json -Compress
    (Get-ScheduledTask -TaskName 'devvm-stage-user').State
}

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1') -Destination C:\ProgramData\devvm -Force
. C:\ProgramData\devvm\lib.ps1
Remove-Item -LiteralPath C:\Users\Public\devvm-user.log, C:\Users\Public\devvm-done-user -ErrorAction SilentlyContinue
Stop-ScheduledTask -TaskName 'devvm-stage-user' -ErrorAction SilentlyContinue
Start-ScheduledTask -TaskName 'devvm-stage-user'
Send-Line 'DEVVM-STAGE-USER-TASK=restarted'

# Until the task has ended: its done file, or the task leaving the Running state after having been seen running.
$seen = $false
while (-not (Test-Path -LiteralPath C:\Users\Public\devvm-done-user)) {
    $state = (Get-ScheduledTask -TaskName 'devvm-stage-user').State
    if ($state -eq 'Running') { $seen = $true }
    elseif ($seen) { break }
    Start-Sleep -Seconds 1
}
Send-Probe 'M4-TASK-AFTER' {
    Get-ScheduledTaskInfo -TaskName 'devvm-stage-user' | Select-Object LastRunTime, LastTaskResult | ConvertTo-Json -Compress
}
if (Test-Path -LiteralPath C:\Users\Public\devvm-user.log) {
    foreach ($line in Get-Content -LiteralPath C:\Users\Public\devvm-user.log -Encoding UTF8) { Send-Line "DEVVM-RELAY: $line" }
} else {
    Send-Line 'DEVVM-RELAY: the user task wrote no log'
}
