# The first-logon script of the base build: run once from the answer disk by Setup's FirstLogonCommands, as the
# administrator `user` (elevated), in Windows PowerShell 5.1. It reports through the serial port (lib.ps1) and ends the
# VM: DEVVM-BASE-DONE and a shutdown, or DEVVM-BASE-FAILED and a shutdown, so a failure is never read as a Setup reboot.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
. (Join-Path $PSScriptRoot 'lib.ps1')

$devvm = @@DEVVM_DIR@@
$scripts = 'C:\Program Files\devvm'
$sshd = 'C:\Program Files\OpenSSH'
$ssh = 'C:\ProgramData\ssh'
$pwsh = @@PWSH_EXE@@
$pwshDir = Split-Path -Parent $pwsh
$brokerExe = @@BROKER_EXE@@
$brokerDir = Split-Path -Parent $brokerExe
$work = Join-Path $env:TEMP 'devvm-dl'

# Downloads and checks the sha256 before anything is extracted.
function Get-Pinned([string]$Url, [string]$Sha256, [string]$To) {
    Invoke-WebRequest -UseBasicParsing -Uri $Url -OutFile $To
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $To).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256) { throw "sha256 of $Url is $actual, expected $Sha256" }
}

# Only SYSTEM and Administrators, as sshd demands of its files.
function Protect-File([string]$Path) {
    icacls.exe $Path /inheritance:r /grant '*S-1-5-18:F' /grant '*S-1-5-32-544:F' | Out-Null
    Assert-Exit "icacls on $Path"
}

$failed = $false
try {
    Send-Line "DEVVM-PROGRESS=first logon: $(whoami.exe) session $([Diagnostics.Process]::GetCurrentProcess().SessionId)"
    New-Item -ItemType Directory -Force -Path $work | Out-Null

    # Where the guest's own files live. The data directory is shared by SYSTEM and `user`, so its ACL is explicit.
    New-Item -ItemType Directory -Force -Path $devvm, $scripts | Out-Null
    icacls.exe $devvm /inheritance:r /grant '*S-1-5-18:(OI)(CI)F' /grant '*S-1-5-32-544:(OI)(CI)F' /grant '*S-1-5-32-545:(OI)(CI)M' | Out-Null
    Assert-Exit "icacls on $devvm"
    foreach ($name in 'lib.ps1', 'boot-task.ps1', 'eb-runner.ps1') {
        Copy-Item -LiteralPath (Join-Path $PSScriptRoot $name) -Destination $scripts -Force
    }
    # The native helper the scripts load: compiled here, once, so that no request and no boot compiles it.
    Add-Type -TypeDefinition (Get-Content -LiteralPath (Join-Path $PSScriptRoot @@NATIVE_SOURCE@@) -Raw) -OutputAssembly (Join-Path $scripts @@NATIVE_DLL@@)

    # The license, read as numbers and identifiers; the host decides what it means. A system no product has a key for (a
    # retail install nobody activated) has no row: that is a read too, and is sent as null.
    $license = Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationId='55c92734-d682-4d71-983e-d6ec3f16059f'" |
        Select-Object -First 1 -Property LicenseStatus, GracePeriodRemaining, Description, LicenseFamily
    $json = if ($license) { $license | ConvertTo-Json -Compress } else { 'null' }
    # ASCII only: the serial channel carries no localized text.
    $json = [regex]::Replace($json, '[^\x00-\x7F]', [System.Text.RegularExpressions.MatchEvaluator]{ param($m) '\u{0:x4}' -f [int][char]$m.Value })
    Send-Line "DEVVM-LICENSE=$json"
    # TEMPORARY DIAGNOSTIC (deputy, devvm#167): the licence state at first logon and at the end. Reverted after one run.
    function Send-Diag([string]$tag) {
        $l = Get-CimInstance SoftwareLicensingProduct -Filter "PartialProductKey IS NOT NULL AND ApplicationId='55c92734-d682-4d71-983e-d6ec3f16059f'" |
            Select-Object LicenseStatus, GracePeriodRemaining, LicenseStatusReason, EvaluationEndDate | ConvertTo-Json -Compress
        $s = (cscript.exe //nologo C:\Windows\System32\slmgr.vbs /dlv) -join ' | '
        $svc = (Get-Service sppsvc).Status
        $net = (Get-NetAdapter | Where-Object Status -eq 'Up' | Measure-Object).Count
        Send-Line ("DEVVM-PROGRESS=diag {0}: {1} sppsvc={2} up={3} {4} {5}" -f $tag, (Get-Date).ToUniversalTime().ToString('o'), $svc, $net, $l, $s)
    }
    Send-Diag 'first-logon'
    Get-CimInstance SoftwareLicensingService | Invoke-CimMethod -MethodName RefreshLicenseStatus | Out-Null
    Send-Diag 'after-refresh'

    # The account: an administrator (by SID, never by the group's localized name) whose password never expires.
    $member = Get-LocalGroupMember -SID S-1-5-32-544 | Where-Object { $_.Name -like '*\user' }
    if (-not $member) { Add-LocalGroupMember -SID S-1-5-32-544 -Member 'user' }
    Set-LocalUser -Name 'user' -PasswordNeverExpires $true
    net.exe accounts /maxpwage:unlimited | Out-Null
    Assert-Exit 'net accounts'
    # `PasswordNeverExpires` is no property of LocalUser; the CIM class says it.
    if ((Get-CimInstance Win32_UserAccount -Filter "Name='user'").PasswordExpires -ne $false) { throw 'the password of user can expire' }

    # Autologon stays on with no logon count, so an interactive session exists at every boot.
    $winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
    Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
    Remove-ItemProperty -Path $winlogon -Name AutoLogonCount -ErrorAction SilentlyContinue

    # No sleep, hibernation (which also ends Fast Startup), lock screen or update restarts; the power button shuts down.
    powercfg.exe /h off
    Assert-Exit 'powercfg /h off'
    powercfg.exe /change standby-timeout-ac 0
    Assert-Exit 'powercfg standby-timeout'
    powercfg.exe /change monitor-timeout-ac 0
    Assert-Exit 'powercfg monitor-timeout'
    powercfg.exe /setacvalueindex SCHEME_CURRENT SUB_BUTTONS PBUTTONACTION 3
    Assert-Exit 'powercfg PBUTTONACTION'
    powercfg.exe /setactive SCHEME_CURRENT
    Assert-Exit 'powercfg /setactive'
    New-Item -Force -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\Personalization' -Name NoLockScreen -Value 1 -Type DWord
    New-Item -Force -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' | Out-Null
    Set-ItemProperty -Path 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' -Name NoAutoRebootWithLoggedOnUsers -Value 1 -Type DWord

    Send-Line 'DEVVM-PROGRESS=installing OpenSSH'
    Get-Pinned @@OPENSSH_URL@@ @@OPENSSH_SHA256@@ "$work\openssh.zip"
    Expand-Archive -LiteralPath "$work\openssh.zip" -DestinationPath "$work\openssh" -Force
    Move-Item -LiteralPath (Get-ChildItem "$work\openssh" -Directory | Select-Object -First 1).FullName -Destination $sshd
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$sshd\install-sshd.ps1"
    Assert-Exit 'install-sshd.ps1'
    # A folder moved from the temp directory keeps that ACL, and sftp-server.exe then cannot start for `user`.
    icacls.exe $sshd /reset /T /C /Q | Out-Null
    Assert-Exit "icacls /reset on $sshd"
    # The boot task starts sshd, after the seed is in place and the agent is up.
    Set-Service -Name sshd -StartupType Manual
    New-NetFirewallRule -Name devvm-sshd -DisplayName 'devvm sshd' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -Profile Any | Out-Null
    # The base holds no host key: the instance's comes from its seed.
    New-Item -ItemType Directory -Force -Path $ssh | Out-Null
    Get-ChildItem -LiteralPath $ssh -Filter 'ssh_host_*' -Force | Remove-Item -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'sshd_config') -Destination "$ssh\sshd_config" -Force
    Protect-File "$ssh\sshd_config"

    Send-Line 'DEVVM-PROGRESS=installing PowerShell'
    Get-Pinned @@POWERSHELL_URL@@ @@POWERSHELL_SHA256@@ "$work\pwsh.zip"
    Expand-Archive -LiteralPath "$work\pwsh.zip" -DestinationPath $pwshDir -Force
    # The zip puts nothing on PATH: the commands devvm runs name `pwsh`, and the agent that starts in a later boot reads
    # the machine value.
    $machinePath = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    if (($machinePath -split ';') -notcontains $pwshDir) {
        [Environment]::SetEnvironmentVariable('Path', "$machinePath;$pwshDir", 'Machine')
    }

    Send-Line 'DEVVM-PROGRESS=installing ssh-broker'
    Get-Pinned @@SSH_BROKER_URL@@ @@SSH_BROKER_SHA256@@ "$work\ssh-broker.zip"
    Expand-Archive -LiteralPath "$work\ssh-broker.zip" -DestinationPath "$work\ssh-broker" -Force
    # `apply` refuses an install directory it did not make, so the config goes in after it; it starts the agent in this
    # session.
    if (Test-Path -LiteralPath $brokerDir) { Remove-Item -LiteralPath $brokerDir -Recurse -Force }
    & "$work\ssh-broker\ssh-broker.exe" apply
    Assert-Exit 'ssh-broker apply'
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'broker-config.yaml') -Destination "$brokerDir\config.yaml" -Force

    # The boot task (every boot, as SYSTEM) and the elevated helper's task (started once per request, as `user` with the
    # highest privileges and an interactive token; its request id is the task's first argument).
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    $bootAction = New-ScheduledTaskAction -Execute $pwsh -Argument ('-NoProfile -NonInteractive -File "' + $scripts + '\boot-task.ps1"')
    Register-ScheduledTask -TaskName 'devvm-boot' -Action $bootAction -Trigger (New-ScheduledTaskTrigger -AtStartup) -Settings $settings `
        -Principal (New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -LogonType ServiceAccount -RunLevel Highest) | Out-Null
    $helperSettings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances Parallel
    $helperAction = New-ScheduledTaskAction -Execute $pwsh -Argument ('-NoProfile -NonInteractive -File "' + $scripts + '\eb-runner.ps1" $(Arg0)')
    New-Item -ItemType Directory -Force -Path @@EB_ROOT@@ | Out-Null
    Register-ScheduledTask -TaskName @@EB_TASK@@ -Action $helperAction -Settings $helperSettings `
        -Principal (New-ScheduledTaskPrincipal -UserId 'user' -LogonType Interactive -RunLevel Highest) | Out-Null

    Send-Diag 'end'
    Remove-Item -LiteralPath $work -Recurse -Force
    Write-VolumeCache -DriveLetter C
} catch {
    $failed = $true
    Send-Line "DEVVM-BASE-FAILED=$($_ | Out-String)"
}

if (-not $failed) { Send-Line 'DEVVM-BASE-DONE' }
shutdown.exe /s /t 0
