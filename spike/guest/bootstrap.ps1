# Spike only (never merged). First-logon bootstrap, run by FirstLogonCommands from the answer disk (PowerShell 5.1).
# Stage 1 of the spike: measure the install (M1, M2, M9, M14, M15), prepare the base and register the stage hooks.
$ErrorActionPreference = 'Stop'

$devvm = 'C:\ProgramData\devvm'
New-Item -ItemType Directory -Force -Path $devvm | Out-Null
$disk = Split-Path -Parent $PSCommandPath
Copy-Item -LiteralPath (Join-Path $disk 'lib.ps1'), (Join-Path $disk 'hook.ps1') -Destination $devvm -Force
. (Join-Path $devvm 'lib.ps1')

$failed = $false
try {
    Send-Line "DEVVM-PROGRESS=first logon: $(whoami) session $(Get-SessionId) integrity $(Get-IntegritySid)"
    Send-Probe 'M1-OS' { Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber, OSLanguage, MUILanguages | ConvertTo-Json -Compress }
    Send-Probe 'M1-ELEVATED' { ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) }
    Send-Probe 'M1-LABCONFIG-CPU' { Get-CimInstance Win32_Processor | Select-Object Name, NumberOfCores, NumberOfLogicalProcessors | ConvertTo-Json -Compress }

    # Network: NetKVM comes from the answer file's DriverPaths; when it did not, install it from the virtio CD.
    $adapter = Get-NetAdapter -ErrorAction SilentlyContinue
    Send-Probe 'NET-ADAPTERS-BEFORE' { Get-NetAdapter | Select-Object Name, InterfaceDescription, Status | ConvertTo-Json -Compress }
    if (-not $adapter) {
        $cd = Get-PSDrive -PSProvider FileSystem | Where-Object { Test-Path -LiteralPath ($_.Root + 'NetKVM\w11\amd64') } | Select-Object -First 1
        if ($cd) {
            Send-Probe 'NET-PNPUTIL' { pnputil /add-driver ($cd.Root + 'NetKVM\w11\amd64\netkvm.inf') /install }
        } else {
            Send-Line 'DEVVM-NET-PNPUTIL=no virtio CD with NetKVM found'
        }
    }
    Send-Probe 'NET-ADAPTERS' { Get-NetAdapter | Select-Object Name, InterfaceDescription, Status | ConvertTo-Json -Compress }
    Send-Probe 'NET-DNS' { [System.Net.Dns]::GetHostAddresses('github.com') | ForEach-Object { $_.ToString() } }

    # M14, M15: the account.
    Send-Probe 'M14-ADMINS' { Get-LocalGroupMember -SID S-1-5-32-544 | Select-Object Name, SID | ConvertTo-Json -Compress }
    $member = Get-LocalGroupMember -SID S-1-5-32-544 | Where-Object { $_.Name -like '*\user' }
    if (-not $member) {
        Send-Line 'DEVVM-M14-ADMIN-BY-ANSWERFILE=no'
        Add-LocalGroupMember -SID S-1-5-32-544 -Member 'user'
    } else {
        Send-Line 'DEVVM-M14-ADMIN-BY-ANSWERFILE=yes'
    }
    Set-LocalUser -Name 'user' -PasswordNeverExpires $true
    net accounts /maxpwage:unlimited | Out-Null
    Send-Probe 'M15-PWNEVEREXPIRES' { (Get-LocalUser -Name user).PasswordNeverExpires }
    Send-Probe 'M14-LOCALE' { Get-WinSystemLocale; Get-WinUserLanguageList | Select-Object LanguageTag, InputMethodTips | ConvertTo-Json -Compress; (Get-TimeZone).Id }

    # Autologon stays on, with no logon count.
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
    Remove-ItemProperty -Path $winlogon -Name AutoLogonCount -ErrorAction SilentlyContinue
    Send-Probe 'WINLOGON' { Get-ItemProperty $winlogon | Select-Object AutoAdminLogon, DefaultUserName, AutoLogonCount, DefaultDomainName | ConvertTo-Json -Compress }

    # No sleep, hibernation (also ends Fast Startup), screen lock or update restarts; the power button shuts down.
    powercfg /h off
    powercfg /change standby-timeout-ac 0
    powercfg /change monitor-timeout-ac 0
    powercfg /setacvalueindex SCHEME_CURRENT SUB_BUTTONS PBUTTONACTION 3
    powercfg /setactive SCHEME_CURRENT
    New-Item -Force -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Name NoLockScreen -Value 1 -Type DWord
    New-Item -Force -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' -Name NoAutoRebootWithLoggedOnUsers -Value 1 -Type DWord
    Send-Probe 'POWERCFG' { powercfg /a }

    # M9: the licensing read, raw.
    Send-Probe 'M9-LICENSE' {
        Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationId='55c92734-d682-4d71-983e-d6ec3f16059f'" |
            Select-Object Name, Description, LicenseStatus, GracePeriodRemaining, LicenseFamily, ProductKeyChannel, PartialProductKey |
            ConvertTo-Json -Compress
    }
    Send-Probe 'M9-SLMGR' { cscript //nologo C:\Windows\System32\slmgr.vbs /dlv }

    # M11, M15: the disk.
    Send-Probe 'DISK' {
        Get-Partition | Select-Object DiskNumber, PartitionNumber, Type, DriveLetter, Size | ConvertTo-Json -Compress
        Get-Volume | Select-Object DriveLetter, FileSystemLabel, FileSystem, Size, SizeRemaining | ConvertTo-Json -Compress
    }

    # The stage hooks: one at boot (SYSTEM), one at logon (the user, Medium integrity).
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    $system = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File $devvm\hook.ps1 -Mode system"
    Register-ScheduledTask -TaskName 'devvm-stage-system' -Action $system -Trigger (New-ScheduledTaskTrigger -AtStartup) `
        -Principal (New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -RunLevel Highest) -Settings $settings | Out-Null
    $logon = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File $devvm\hook.ps1 -Mode user"
    Register-ScheduledTask -TaskName 'devvm-stage-user' -Action $logon -Trigger (New-ScheduledTaskTrigger -AtLogOn -User 'user') `
        -Principal (New-ScheduledTaskPrincipal -UserId 'user' -LogonType Interactive -RunLevel Limited) -Settings $settings | Out-Null
    Send-Probe 'TASKS' { Get-ScheduledTask -TaskName 'devvm-stage-*' | Select-Object TaskName, State | ConvertTo-Json -Compress }
} catch {
    $failed = $true
    Send-Line "DEVVM-BASE-FAILED=$($_ | Out-String)"
}

Write-VolumeCache -DriveLetter C -ErrorAction SilentlyContinue
if (-not $failed) {
    Send-Line 'DEVVM-BASE-DONE'
}
shutdown.exe /s /t 0
