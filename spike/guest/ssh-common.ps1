# Spike only (never merged). The steps of the SSH-driven stages' boot task (SYSTEM), dot-sourced by their stage-system.ps1:
# what the product's seed task would do, then sshd. PowerShell 5.1. Needs lib.ps1 (Send-Line) first.

function Step([string]$Name, [scriptblock]$Block) {
    $began = Get-Date
    try {
        $out = (& $Block 2>&1 | Out-String)
        Send-Line "DEVVM-STEP-$Name=ok seconds=$([int]((Get-Date) - $began).TotalSeconds) $out"
    } catch {
        Send-Line "DEVVM-STEP-$Name=FAILED seconds=$([int]((Get-Date) - $began).TotalSeconds) $($_ | Out-String)"
    }
}

# C: into the larger disk. A partition after C: (Setup's recovery partition) blocks the growth, so it goes first.
function Invoke-ExtendC {
    $disk = (Get-Partition -DriveLetter C).DiskNumber
    $c = Get-Partition -DriveLetter C
    Get-Partition -DiskNumber $disk | Where-Object { $_.Offset -gt $c.Offset } | ForEach-Object {
        "removing partition $($_.PartitionNumber) type $($_.Type)"
        Remove-Partition -DiskNumber $disk -PartitionNumber $_.PartitionNumber -Confirm:$false
    }
    Resize-Partition -DriveLetter C -Size (Get-PartitionSupportedSize -DriveLetter C).SizeMax
    "after: C: $((Get-Partition -DriveLetter C).Size)"
}

# `apply` on an absent install directory (a pre-written one is refused), then the shell config the stages use.
function Invoke-BrokerApply([string]$Root) {
    $install = 'C:\ProgramData\ssh-broker'
    & (Join-Path $Root 'ssh-broker.exe') apply 2>&1 | Out-String
    "apply exit code $LASTEXITCODE"
    Copy-Item -LiteralPath (Join-Path $Root 'config.yaml') -Destination "$install\config.yaml" -Force
}

# The host key pair and authorized keys from the stage disk, with the ACLs sshd demands (SYSTEM and Administrators only).
# The hash before and after says whether a key from an earlier boot was there.
function Invoke-Seed([string]$Root) {
    $ssh = 'C:\ProgramData\ssh'
    New-Item -ItemType Directory -Force -Path $ssh | Out-Null
    $files = @{
        'ssh_host_ed25519_key'     = 'ssh_host_ed25519_key'
        'ssh_host_ed25519_key.pub' = 'ssh_host_ed25519_key.pub'
        'authorized_keys'          = 'administrators_authorized_keys'
    }
    foreach ($name in $files.Keys | Sort-Object) {
        $target = Join-Path $ssh $files[$name]
        $before = if (Test-Path -LiteralPath $target) { (Get-FileHash -Algorithm SHA256 -LiteralPath $target).Hash } else { 'absent' }
        $seed = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Root $name)).Hash
        "$($files[$name]): on disk before $before; seed $seed"
        Copy-Item -LiteralPath (Join-Path $Root $name) -Destination $target -Force
        icacls $target /inheritance:r /grant '*S-1-5-18:F' /grant '*S-1-5-32-544:F' | Out-Null
    }
}

# $LogLevel '' leaves sshd's default log level (no log file).
function Invoke-SshdStart([string]$LogLevel = 'DEBUG1') {
    # More of sshd's own account of a connection, for the watcher. The event log channel (OpenSSH/Operational) stayed
    # empty on both guests (run 43260908167), so: `SyslogFacility LOCAL0` makes sshd write C:\ProgramData\ssh\logs\sshd.log.
    $config = 'C:\ProgramData\ssh\sshd_config'
    if ($LogLevel -and -not (Test-Path -LiteralPath $config)) {
        # The base holds no sshd_config: the service writes it from sshd_config_default when it first starts (run
        # 43278227165: the edit below was skipped and the DEBUG3 log never existed). Let it write it, then edit.
        Start-Service sshd
        while (-not (Test-Path -LiteralPath $config)) { Start-Sleep -Milliseconds 200 }  # pacing; the file is the condition
        Stop-Service sshd
    }
    if ($LogLevel -and (Test-Path -LiteralPath $config)) {
        $lines = [System.Collections.Generic.List[string]](Get-Content -LiteralPath $config)
        $lines.RemoveAll({ param($line) $line -match '^\s*(LogLevel|SyslogFacility)\s' }) | Out-Null
        # Before the first Match block: the keywords belong to the global part.
        $at = $lines.FindIndex({ param($line) $line -match '^\s*Match\s' })
        if ($at -lt 0) { $at = $lines.Count }
        $lines.Insert($at, "LogLevel $LogLevel")
        $lines.Insert($at, 'SyslogFacility LOCAL0')
        Set-Content -LiteralPath $config -Value $lines -Encoding ascii
    }
    Start-Service sshd
    Get-Service sshd | Select-Object Name, Status, StartType | Out-String
    Select-String -Path 'C:\ProgramData\ssh\sshd_config' -Pattern '^\s*(Subsystem|Match|ForceCommand|AuthorizedKeysFile|PubkeyAuthentication|HostKey|LogLevel|SyslogFacility)' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line }
}

# The guest's own view next to the host's measurements: watch.ps1 as a SYSTEM task, one line per 10 s on the serial log.
function Start-Watcher {
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\watch.ps1'
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName 'devvm-watch' -Action $action -Settings $settings -Force `
        -Principal (New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest) | Out-Null
    Start-ScheduledTask -TaskName 'devvm-watch'
    (Get-ScheduledTask -TaskName 'devvm-watch').State
}

# E1 of the wedge root-cause run: hangdump.ps1 as a SYSTEM task. It waits for a stalled transfer, then dumps sshd and
# sshd-session (procdump64.exe from the stage disk) and sends the dumps and logs to the host (wedge_driver.py).
function Start-HangDump([string]$Root, [bool]$Capture) {
    Copy-Item -LiteralPath (Join-Path $Root 'hangdump.ps1'), (Join-Path $Root 'procdump64.exe') -Destination C:\ProgramData\devvm -Force
    $arguments = '-NoProfile -ExecutionPolicy Bypass -File C:\ProgramData\devvm\hangdump.ps1'
    if ($Capture) { $arguments += ' -Pkt' }
    $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $arguments
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName 'devvm-hangdump' -Action $action -Settings $settings -Force `
        -Principal (New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest) | Out-Null
    Start-ScheduledTask -TaskName 'devvm-hangdump'
    (Get-ScheduledTask -TaskName 'devvm-hangdump').State
}

# An agent on demand (no trigger; the host starts it): elevated for M6 E-C, plain to put the Medium agent back afterwards.
function Register-AgentTask([string]$Name, [string]$RunLevel) {
    $action = New-ScheduledTaskAction -Execute 'C:\ProgramData\ssh-broker\ssh-broker.exe' -Argument 'agent'
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances Parallel
    Register-ScheduledTask -TaskName $Name -Action $action -Settings $settings -Force `
        -Principal (New-ScheduledTaskPrincipal -UserId 'user' -LogonType Interactive -RunLevel $RunLevel) | Out-Null
    (Get-ScheduledTask -TaskName $Name).State
}

# The elevated scheduled-task helper of M6 E-B: the request directory the Medium session writes into, and the task.
function Register-EbHelper([string]$Root) {
    $dir = 'C:\ProgramData\devvm\eb'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    icacls $dir /grant 'user:(OI)(CI)F' | Out-Null
    Copy-Item -LiteralPath (Join-Path $Root 'eb-runner.ps1') -Destination "$dir\eb-runner.ps1" -Force
    $action = New-ScheduledTaskAction -Execute 'C:\Program Files\PowerShell\7\pwsh.exe' -Argument "-NoProfile -NonInteractive -File $dir\eb-runner.ps1"
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName 'devvm-eb' -Action $action -Settings $settings -Force `
        -Principal (New-ScheduledTaskPrincipal -UserId 'user' -LogonType Interactive -RunLevel Highest) | Out-Null
    (Get-ScheduledTask -TaskName 'devvm-eb').State
}

# E-B with one request per start (M6, "two requests at once"): `devvm-eb2` runs eb-runner2.ps1 with the request
# directory as `$(Arg0)` and lets instances run in parallel. The caller starts it through the Task Scheduler COM API
# (`Run($dir)`), which `Start-ScheduledTask` cannot do.
function Register-EbParallelHelper([string]$Root) {
    $dir = 'C:\ProgramData\devvm\eb'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    icacls $dir /grant 'user:(OI)(CI)F' | Out-Null
    Copy-Item -LiteralPath (Join-Path $Root 'eb-runner2.ps1') -Destination "$dir\eb-runner2.ps1" -Force
    $action = New-ScheduledTaskAction -Execute 'C:\Program Files\PowerShell\7\pwsh.exe' -Argument "-NoProfile -NonInteractive -File $dir\eb-runner2.ps1 `$(Arg0)"
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances Parallel
    Register-ScheduledTask -TaskName 'devvm-eb2' -Action $action -Settings $settings -Force `
        -Principal (New-ScheduledTaskPrincipal -UserId 'user' -LogonType Interactive -RunLevel Highest) | Out-Null
    $task = Get-ScheduledTask -TaskName 'devvm-eb2'
    "$($task.State) multiple instances: $($task.Settings.MultipleInstances) arguments: $($task.Actions[0].Arguments)"
}
