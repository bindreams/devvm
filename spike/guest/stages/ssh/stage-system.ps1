# Spike only (never merged). Stage `ssh`, boot task (SYSTEM): what the product's seed task would do, then sshd.
#   M3:     the host key pair and the client's authorized key come from this disk; sshd starts after them.
#   M3/M15: C: extends into a larger disk (the workflow resizes the overlay).
#   M4:     `ssh-broker apply` with a config.yaml written first; the E-C helper task (an elevated agent, started on demand).
# The host runs `ssh_driver.py` against this guest and asks for the power-down when it ends.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'probe.ps1'), (Join-Path $Root 'killagent.ps1') -Destination C:\ProgramData\devvm -Force

function Step([string]$Name, [scriptblock]$Block) {
    $began = Get-Date
    try {
        $out = (& $Block 2>&1 | Out-String)
        Send-Line "DEVVM-STEP-$Name=ok seconds=$([int]((Get-Date) - $began).TotalSeconds) $out"
    } catch {
        Send-Line "DEVVM-STEP-$Name=FAILED seconds=$([int]((Get-Date) - $began).TotalSeconds) $($_ | Out-String)"
    }
}

# C: into the larger disk. A partition after C: (Setup's recovery partition) blocks the growth; the second try removes it.
Step 'EXTEND-C' {
    $disk = (Get-Partition -DriveLetter C).DiskNumber
    "before: disk $((Get-Disk -Number $disk).Size) C: $((Get-Partition -DriveLetter C).Size) supported max $((Get-PartitionSupportedSize -DriveLetter C).SizeMax)"
    Get-Partition -DiskNumber $disk | Select-Object PartitionNumber, Type, Offset, Size | Format-Table | Out-String
    $max = (Get-PartitionSupportedSize -DriveLetter C).SizeMax
    # More than rounding: the room is there, not just alignment slack.
    if ($max - (Get-Partition -DriveLetter C).Size -gt 1GB) {
        Resize-Partition -DriveLetter C -Size $max
        'extended without removing anything'
    } else {
        'C: cannot grow as it is; removing the partitions after it'
        $c = Get-Partition -DriveLetter C
        Get-Partition -DiskNumber $disk | Where-Object { $_.Offset -gt $c.Offset } | ForEach-Object {
            "removing partition $($_.PartitionNumber) type $($_.Type)"
            Remove-Partition -DiskNumber $disk -PartitionNumber $_.PartitionNumber -Confirm:$false
        }
        Resize-Partition -DriveLetter C -Size (Get-PartitionSupportedSize -DriveLetter C).SizeMax
    }
    "after: C: $((Get-Partition -DriveLetter C).Size) free $((Get-Volume -DriveLetter C).SizeRemaining)"
}

# The broker: config.yaml first, as the plan says; a refusal is recorded and `apply` runs on a clean directory.
$broker = Join-Path $Root 'ssh-broker.exe'
$install = 'C:\ProgramData\ssh-broker'
Step 'BROKER-APPLY-PREWRITTEN-CONFIG' {
    New-Item -ItemType Directory -Force -Path $install | Out-Null
    Copy-Item -LiteralPath (Join-Path $Root 'config.yaml') -Destination "$install\config.yaml" -Force
    & $broker apply 2>&1 | Out-String
    "apply exit code $LASTEXITCODE"
}
if (-not (Test-Path -LiteralPath "$install\ssh-broker.exe")) {
    Step 'BROKER-APPLY-CLEAN' {
        Remove-Item -LiteralPath $install -Recurse -Force
        & $broker apply 2>&1 | Out-String
        "apply exit code $LASTEXITCODE"
        Copy-Item -LiteralPath (Join-Path $Root 'config.yaml') -Destination "$install\config.yaml" -Force
    }
}
Step 'BROKER-STATE' {
    Get-ChildItem $install | Select-Object Name, Length | Out-String
    Get-ItemProperty 'HKLM:\SOFTWARE\OpenSSH' | Select-Object DefaultShell, DefaultShellCommandOption, DefaultShellEscapeArguments | Out-String
    Get-ScheduledTask | Where-Object { $_.TaskName -like '*ssh-broker*' -or $_.TaskPath -like '*ssh-broker*' } | Select-Object TaskPath, TaskName, State | Out-String
    icacls $install
}

# E-C: an elevated agent on demand (no trigger; the SSH driver starts it).
Step 'ELEVATED-AGENT-TASK' {
    $action = New-ScheduledTaskAction -Execute "$install\ssh-broker.exe" -Argument 'agent'
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -MultipleInstances Parallel
    Register-ScheduledTask -TaskName 'devvm-agent-elevated' -Action $action -Settings $settings `
        -Principal (New-ScheduledTaskPrincipal -UserId 'user' -LogonType Interactive -RunLevel Highest) | Out-Null
    (Get-ScheduledTask -TaskName 'devvm-agent-elevated').State
}

# The seed: host key pair and authorized keys, with the ACLs sshd demands (SYSTEM and Administrators only).
$ssh = 'C:\ProgramData\ssh'
Step 'SEED' {
    New-Item -ItemType Directory -Force -Path $ssh | Out-Null
    $files = @{
        'ssh_host_ed25519_key'     = 'ssh_host_ed25519_key'
        'ssh_host_ed25519_key.pub' = 'ssh_host_ed25519_key.pub'
        'authorized_keys'          = 'administrators_authorized_keys'
    }
    foreach ($name in $files.Keys) {
        $target = Join-Path $ssh $files[$name]
        Copy-Item -LiteralPath (Join-Path $Root $name) -Destination $target -Force
        icacls $target /inheritance:r /grant '*S-1-5-18:F' /grant '*S-1-5-32-544:F' | Out-Null
        icacls $target
    }
    Get-ChildItem $ssh | Select-Object Name, Length | Out-String
}
Step 'SSHD-START' {
    Start-Service sshd
    Get-Service sshd | Select-Object Name, Status, StartType | Out-String
    Get-NetTCPConnection -LocalPort 22 -State Listen | Select-Object LocalAddress, LocalPort, OwningProcess | Out-String
    Get-ChildItem $ssh | Select-Object Name, Length | Out-String
}
Send-Line 'DEVVM-SSH-STARTED'
