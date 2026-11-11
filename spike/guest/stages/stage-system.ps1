# Spike only (never merged). Stage `boot1`, boot task (SYSTEM): what a boot looks like before anyone is logged on.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1

$boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
Send-Line "DEVVM-M15-BOOTTIME=$($boot.ToUniversalTime().ToString('o')) uptime-seconds=$([int]((Get-Date) - $boot).TotalSeconds)"
Send-Line "DEVVM-STAGE-WHO=$(whoami) session $(Get-SessionId) integrity $(Get-IntegritySid)"
Send-Probe 'M4-SESSIONS' { query user 2>&1; query session 2>&1 }
