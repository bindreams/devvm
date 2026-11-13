# Spike only (never merged). Stage `ptywin`, boot task (SYSTEM), on every boot of the stage's two launches:
#   B1.3  why an SSH exec request with a pty never runs its command (the driver, ptywin_driver.py, makes the requests);
#         this script gives it process auditing (tracer.ps1: every process started and ended, with command lines and exit
#         statuses), sshd at DEBUG3, and the broker's own logs (the user's AppData\Local\ssh-broker\logs).
#   Windows  does the agent's console window show, as the logon task starts it? `winwatch.exe` (winwatch.cs) lists the
#         top-level windows of session 1 from a task that runs there, because a window enumeration sees only the caller's
#         desktop and SYSTEM's is session 0's. Launch 1: the agent that `apply` starts; launch 2: the agent the logon
#         trigger starts, with the helper started by the same logon.
# First boot: process auditing, `ssh-broker apply`, the helper's build and task. Every boot: seed, sshd, watcher, tracer.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
. (Join-Path $Root 'ssh-common.ps1')

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'probe.ps1'), (Join-Path $Root 'watch.ps1'), `
    (Join-Path $Root 'tracer.ps1'), (Join-Path $Root 'terminal-facts.ps1') -Destination C:\ProgramData\devvm -Force
. C:\ProgramData\devvm\lib.ps1  # the copy sends the time with every line

# Security 4688 (with the command line) and 4689 (with the exit status). By GUID: the subcategory names are localized.
function Enable-ProcessAudit {
    foreach ($guid in '{0CCE922B-69AE-11D9-BED3-505054503030}', '{0CCE922C-69AE-11D9-BED3-505054503030}') {
        auditpol /set /subcategory:$guid /success:enable /failure:enable
        auditpol /get /subcategory:$guid
    }
    $key = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System\Audit'
    New-Item -Path $key -Force | Out-Null
    New-ItemProperty -Path $key -Name ProcessCreationIncludeCmdLine_Enabled -PropertyType DWord -Value 1 -Force | Out-Null
    "command line auditing: $((Get-ItemProperty -Path $key).ProcessCreationIncludeCmdLine_Enabled)"
}

function Register-WinWatch {
    $dir = 'C:\ProgramData\devvm\win'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    icacls $dir /grant 'user:(OI)(CI)F' | Out-Null
    Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'winwatch.cs')) -OutputType WindowsApplication -OutputAssembly C:\ProgramData\devvm\winwatch.exe
    $action = New-ScheduledTaskAction -Execute 'C:\ProgramData\devvm\winwatch.exe'
    $trigger = New-ScheduledTaskTrigger -AtLogOn -User 'user'
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName 'devvm-winwatch' -Action $action -Trigger $trigger -Settings $settings -Force `
        -Principal (New-ScheduledTaskPrincipal -UserId 'user' -LogonType Interactive -RunLevel Limited) | Out-Null
    (Get-ScheduledTask -TaskName 'devvm-winwatch').State
}

# The trigger starts it at a logon that happens after the registration; the first boot's logon is usually earlier. The wait is
# for the user's explorer (the logon itself); `IgnoreNew` makes a second start by the trigger a no-op.
function Start-WinWatch {
    while (-not (Get-Process -Name explorer -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq 1 })) { Start-Sleep -Milliseconds 500 }  # pacing; explorer is the condition
    Start-ScheduledTask -TaskName 'devvm-winwatch'
    (Get-ScheduledTask -TaskName 'devvm-winwatch').State
}

function Start-Tracer {
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\tracer.ps1'
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName 'devvm-tracer' -Action $action -Settings $settings -Force `
        -Principal (New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest) | Out-Null
    Start-ScheduledTask -TaskName 'devvm-tracer'
    (Get-ScheduledTask -TaskName 'devvm-tracer').State
}

$marker = 'C:\ProgramData\devvm\ptywin-first-boot-done'
$first = -not (Test-Path -LiteralPath $marker)
if ($first) {
    Send-Line 'DEVVM-BOOT=first'
    Step 'AUDIT' { Enable-ProcessAudit }
    Step 'BROKER-APPLY' { Invoke-BrokerApply $Root }
    Step 'SSHD-AUTOMATIC' { Set-Service -Name sshd -StartupType Automatic; (Get-Service sshd).StartType.ToString() }
    Step 'TERMINAL-PROVISIONED' {
        Get-AppxPackage -AllUsers -Name 'Microsoft.WindowsTerminal*' | Select-Object Name, Version, InstallLocation | Format-List
        Get-AppxProvisionedPackage -Online | Where-Object { $_.DisplayName -match 'Terminal' } | Select-Object DisplayName, Version | Format-List
    }
    Step 'WINWATCH-REGISTER' { Register-WinWatch }
    Set-Content -LiteralPath $marker -Value (Get-Date -Format o)
} else {
    Send-Line 'DEVVM-BOOT=later'
}
Step 'SEED' { Invoke-Seed $Root }
Step 'SSHD-START' { Invoke-SshdStart 'DEBUG3' }
Send-Line 'DEVVM-SSH-STARTED'
Step 'WATCHER' { Start-Watcher }
Step 'TRACER' { Start-Tracer }
Step 'AGENT-TASK' { Get-ScheduledTask | Where-Object { $_.TaskName -like '*ssh-broker*' -or $_.TaskPath -like '*ssh-broker*' } | Select-Object TaskPath, TaskName, State | Out-String }
if ($first) { Step 'WINWATCH-START' { Start-WinWatch } }
