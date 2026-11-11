# Spike only (never merged). Stage `boot1`, logon task (the user, Medium): is a session 1 reached with the account the
# plan expects, and what does a Medium-integrity process see?
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1

$boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
Send-Line "DEVVM-M11-LOGON-UPTIME-SECONDS=$([int]((Get-Date) - $boot).TotalSeconds)"
Send-Line "DEVVM-STAGE-WHO=$(whoami) session $(Get-SessionId) integrity $(Get-IntegritySid)"
Send-Probe 'M4-WHOAMI-GROUPS' { whoami /groups /fo csv }
Send-Probe 'M4-ELEVATED' { ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) }
Send-Probe 'M15-PWNEVEREXPIRES' { (Get-LocalUser -Name user).PasswordNeverExpires }
Send-Probe 'M15-WRITEVOLUMECACHE-MEDIUM' { Write-VolumeCache -DriveLetter C -ErrorAction Stop; 'worked' }
Send-Probe 'M15-LASTBOOT' { (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o') }
Send-Probe 'ENV' { [Environment]::GetEnvironmentVariables().GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" } }
Send-Probe 'M13-WINGET' { winget --version }
Send-Probe 'M13-FIREWALL' { Get-NetConnectionProfile | Select-Object Name, NetworkCategory, InterfaceAlias | ConvertTo-Json -Compress }
