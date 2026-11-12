# Spike only (never merged). Prints what the session that runs it is: one `key=value` per line. PowerShell 5.1, no
# parameters, nothing localized: the SSH driver parses it.
$ErrorActionPreference = 'Continue'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$self = Get-CimInstance Win32_Process -Filter "ProcessId=$PID"
$parent = Get-CimInstance Win32_Process -Filter "ProcessId=$($self.ParentProcessId)"
$grand = if ($parent) { Get-CimInstance Win32_Process -Filter "ProcessId=$($parent.ParentProcessId)" }
"session=$([Diagnostics.Process]::GetCurrentProcess().SessionId)"
"sid=$($identity.User.Value)"
"integrity=$((whoami /groups /fo csv | ConvertFrom-Csv | Where-Object { $_.SID -like 'S-1-16-*' } | Select-Object -First 1).SID)"
"elevated=$(([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
"hkcu=$(Test-Path 'HKCU:\Software')"
"boot=$((Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks)"
"parent=$($parent.Name):$($parent.CommandLine)"
"grandparent=$($grand.Name)"
"userprofile=$env:USERPROFILE"
"localappdata=$env:LOCALAPPDATA"
try {
    Add-Type -AssemblyName System.Security
    $blob = [Security.Cryptography.ProtectedData]::Protect([Text.Encoding]::UTF8.GetBytes('devvm'), $null, 'CurrentUser')
    "dpapi=$([Text.Encoding]::UTF8.GetString([Security.Cryptography.ProtectedData]::Unprotect($blob, $null, 'CurrentUser')))"
} catch {
    "dpapi=ERROR $($_.Exception.Message -replace '\s+', ' ')"
}
# UTC ticks (100 ns since 0001-01-01), so the host can order the guest's events: the window between sshd answering and the agent.
"now=$([DateTime]::UtcNow.Ticks)"
"sshd-created=$((Get-CimInstance Win32_Process -Filter "Name='sshd.exe'" | ForEach-Object { "$($_.ProcessId):$($_.CreationDate.ToUniversalTime().Ticks)" }) -join ',')"
"explorer-created=$((Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | ForEach-Object { "$($_.ProcessId):s$($_.SessionId):$($_.CreationDate.ToUniversalTime().Ticks)" }) -join ',')"
"agents=$((Get-CimInstance Win32_Process -Filter "Name='ssh-broker.exe'" | ForEach-Object { "$($_.ProcessId):s$($_.SessionId):t$($_.CreationDate.ToUniversalTime().Ticks):$($_.CommandLine)" }) -join ' ; ')"
