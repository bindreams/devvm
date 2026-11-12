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

function Invoke-SshdStart {
    Start-Service sshd
    Get-Service sshd | Select-Object Name, Status, StartType | Out-String
    Select-String -Path 'C:\ProgramData\ssh\sshd_config' -Pattern '^\s*(Subsystem|Match|ForceCommand|AuthorizedKeysFile|PubkeyAuthentication|HostKey)' -ErrorAction SilentlyContinue | ForEach-Object { $_.Line }
}
