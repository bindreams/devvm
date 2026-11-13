# The boot task: PowerShell 7 as SYSTEM, at every boot. It applies the instance's seed (the CIDATA disk), makes C: as large
# as the disk, and starts sshd last, once the agent of ssh-broker answers. sshd is a Manual service, so nothing is served
# before this ends well: a step that fails writes DEVVM-BOOT-FAILED and leaves sshd stopped (fail closed), and the host's
# boot bound then ends the wait. Stopping and starting the instance tries again.
$ErrorActionPreference = 'Stop'
@@!UTF8_OUTPUT@@
. (Join-Path $PSScriptRoot 'lib.ps1')

$root = @@EB_ROOT@@
$bootIdFile = @@BOOT_ID_FILE@@
$ssh = 'C:\ProgramData\ssh'
# The seed's files and where each goes; an administrator's keys are read from administrators_authorized_keys.
$seed = [ordered]@{
    'ssh_host_ed25519_key'     = 'ssh_host_ed25519_key'
    'ssh_host_ed25519_key.pub' = 'ssh_host_ed25519_key.pub'
    'authorized_keys'          = 'administrators_authorized_keys'
}

# A file's bytes on the disk, whole or not at all: a temp file next to it, flushed to the disk, then renamed over it.
function Write-Whole([string]$Target, [byte[]]$Bytes, [string[]]$Grants) {
    $temp = "$Target.devvm-tmp"
    $stream = [IO.File]::Open($temp, 'Create', 'Write', 'None')
    try { $stream.Write($Bytes, 0, $Bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
    icacls.exe $temp /inheritance:r @Grants | Out-Null
    Assert-Exit "icacls on $temp"
    [IO.File]::Move($temp, $Target, $true)
}

# Whether the agent of `user` accepts a connection on its socket, which is in the user's profile (found by SID: the
# profile list names it). A connect that succeeds is the condition under which the shim does not fail open.
function Test-AgentSocket([string]$Sid) {
    $profile = (Get-ItemProperty -LiteralPath "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$Sid" -ErrorAction SilentlyContinue).ProfileImagePath
    if (-not $profile) { return $false }
    $socket = [Net.Sockets.Socket]::new([Net.Sockets.AddressFamily]::Unix, [Net.Sockets.SocketType]::Stream, [Net.Sockets.ProtocolType]::Unspecified)
    try {
        $socket.Connect([Net.Sockets.UnixDomainSocketEndPoint]::new((Join-Path $profile 'AppData\Local\ssh-broker\agent.sock')))
        return $true
    } catch [Net.Sockets.SocketException] {
        return $false
    } finally {
        $socket.Dispose()
    }
}

try {
    # Requests the last boot left behind: no caller survives a boot, so every one is stale. Nothing else is removed.
    if (Test-Path -LiteralPath $root) { Get-ChildItem -LiteralPath $root -Force | Remove-Item -Recurse -Force }

    # The disk may appear after the boot has started.
    while (-not ($disk = Find-VolumeRoot 'CIDATA')) { Start-Sleep -Milliseconds 200 }

    New-Item -ItemType Directory -Force -Path $ssh | Out-Null
    $grants = '/grant', '*S-1-5-18:F', '/grant', '*S-1-5-32-544:F'
    foreach ($name in $seed.Keys) {
        $target = Join-Path $ssh $seed[$name]
        Write-Whole $target ([IO.File]::ReadAllBytes((Join-Path $disk $name))) $grants
        Send-Line "DEVVM-SEED-WRITTEN=$name"
    }
    # A write a reset cut can come back as zeros: each file is read again and must equal the seed's.
    foreach ($name in $seed.Keys) {
        $target = Join-Path $ssh $seed[$name]
        $wanted = [IO.File]::ReadAllBytes((Join-Path $disk $name))
        $actual = [IO.File]::ReadAllBytes($target)
        if ([Convert]::ToBase64String($actual) -cne [Convert]::ToBase64String($wanted)) { throw "$target differs from the seed's $name after it was written" }
    }

    # This boot's id, before sshd starts, so that any command that reaches the guest reads it.
    $bytes = [byte[]]::new(16)
    [Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
    Write-Whole $bootIdFile ([Text.Encoding]::ASCII.GetBytes([Convert]::ToHexString($bytes).ToLowerInvariant())) ('/grant', '*S-1-5-18:F', '/grant', '*S-1-5-32-544:F', '/grant', '*S-1-5-32-545:R')

    # C: as large as the disk. No partition follows it (the base has no recovery partition), so it can grow; a C: that is
    # as large as the disk needs nothing.
    $largest = (Get-PartitionSupportedSize -DriveLetter C).SizeMax
    if ($largest - (Get-Partition -DriveLetter C).Size -ge 1MB) { Resize-Partition -DriveLetter C -Size $largest }

    # No cap: if autologon fails the agent never comes and the host's boot bound ends the wait.
    $sid = ([Security.Principal.NTAccount]::new('user')).Translate([Security.Principal.SecurityIdentifier]).Value
    while (-not (Test-AgentSocket $sid)) { Start-Sleep -Milliseconds 200 }

    Start-Service -Name sshd
} catch {
    Send-Line "DEVVM-BOOT-FAILED=$($_ | Out-String)"
    exit 1
}
