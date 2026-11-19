# The boot task: PowerShell 7 as SYSTEM, at every boot. It names the computer after the instance, applies the instance's
# seed (the CIDATA disk), makes C: as large as the disk, and starts sshd last, once the agent of ssh-broker answers.
# sshd is a Manual service, so a failed step leaves it stopped (fail closed).
$ErrorActionPreference = 'Stop'
@@!UTF8_OUTPUT@@
. (Join-Path $PSScriptRoot 'lib.ps1')
Add-Type -LiteralPath (Join-Path $PSScriptRoot @@NATIVE_DLL@@)

$root = @@EB_ROOT@@
$bootIdFile = @@BOOT_ID_FILE@@
# The name the last boot restarted for: a boot that still has another name finds that the rename did not take.
$renamedFile = Join-Path @@DEVVM_DIR@@ 'renamed-to'
$ssh = 'C:\ProgramData\ssh'
# The seed's files and where each goes; an administrator's keys are read from administrators_authorized_keys.
$seed = [ordered]@{
    'ssh_host_ed25519_key'     = 'ssh_host_ed25519_key'
    'ssh_host_ed25519_key.pub' = 'ssh_host_ed25519_key.pub'
    'authorized_keys'          = 'administrators_authorized_keys'
}

# A file's bytes on the disk, whole or not at all: a temp file next to it, created with its ACL (nothing inherited, so no
# one else can open it and no byte is written before the ACL holds), flushed to the disk, then renamed over the target,
# and the directory flushed. SYSTEM and Administrators get full control; the SIDs in $ReadSids may read.
function Write-Whole([string]$Target, [byte[]]$Bytes, [string[]]$ReadSids = @()) {
    $temp = "$Target.devvm-tmp"
    $rights = [Security.AccessControl.FileSystemRights]
    $allow = [Security.AccessControl.AccessControlType]::Allow
    $security = [Security.AccessControl.FileSecurity]::new()
    $security.SetAccessRuleProtection($true, $false)
    foreach ($sid in 'S-1-5-18', 'S-1-5-32-544') {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid), $rights::FullControl, $allow))
    }
    foreach ($sid in $ReadSids) {
        $security.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($sid), $rights::Read, $allow))
    }
    # A temp file left by a boot that was cut short has its own ACL: it is removed, never reused.
    [IO.File]::Delete($temp)
    $stream = [IO.FileSystemAclExtensions]::Create([IO.FileInfo]::new($temp), [IO.FileMode]::CreateNew, $rights::Write, [IO.FileShare]::None, 4096, [IO.FileOptions]::None, $security)
    try { $stream.Write($Bytes, 0, $Bytes.Length); $stream.Flush($true) } finally { $stream.Dispose() }
    [IO.File]::Move($temp, $Target, $true)
    [Devvm.Disk]::FlushDirectory((Split-Path -Parent $Target))
}

# Restarts now, retrying while the system refuses as not ready (21, 1722, 1190; each distinct status logged once). Any
# other refusal ends the boot. 1190 means a shutdown is scheduled: it is aborted, then the request is retried. One
# already in progress (1115) counts as accepted. Status codes, not localized text, are read.
function Restart-Now {
    $seen = @()
    while ($true) {
        shutdown.exe /r /f /t 0 | Out-Null
        $status = $LASTEXITCODE
        if ($status -in 0, 1115) { return }
        if ($status -notin 21, 1722, 1190) { throw "shutdown.exe /r ended with status $status" }
        if ($status -notin $seen) {
            Send-Line "DEVVM-RESTART-REFUSED=$status"
            $seen += $status
        }
        if ($status -eq 1190) { shutdown.exe /a | Out-Null }
        Start-Sleep -Milliseconds 200
    }
}

# The pending computer name on the disk: Rename-Computer writes it to the registry, which is flushed lazily.
function Save-ComputerName {
    $names = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SYSTEM\CurrentControlSet\Control\ComputerName\ComputerName')
    try { $names.Flush() } finally { $names.Dispose() }
}

# Names the computer after the seed's `local-hostname`, the one in the disk's meta.json. A name takes effect at a restart:
# a computer with another name is renamed and restarted, and the result is $true (the boot ends there). The result is
# the function's only output. $Marker holds the name the last boot restarted for.
function Set-ComputerNameFromSeed([string]$Disk, [string]$Marker) {
    $hostname = (Get-Content -LiteralPath (Join-Path $Disk 'meta.json') -Raw | ConvertFrom-Json).'local-hostname'
    if (-not $hostname) { throw "the seed's meta.json has no local-hostname" }
    if ($env:COMPUTERNAME -eq $hostname) { return $false }
    if ((Test-Path -LiteralPath $Marker) -and ((Get-Content -LiteralPath $Marker -Raw) -eq $hostname)) {
        throw "the computer is still named $env:COMPUTERNAME after the rename to $hostname and a restart"
    }
    $null = Rename-Computer -NewName $hostname -Force
    # The pending name is on the disk before the marker says a restart is owed for it.
    $null = Save-ComputerName
    $null = Write-Whole $Marker ([Text.Encoding]::UTF8.GetBytes($hostname))
    $null = Send-Line "DEVVM-RENAMED=$hostname"
    $null = Restart-Now
    return $true
}

@@!EB_REMOVE_REQUEST@@
# What the last boot left in the root: no caller survives a boot, so every request is stale, and nothing else is kept.
# Each leftover is tried once: a link is removed as a link, never followed (the root is writable by Users). One that
# cannot be removed now (a file held open by a scanner, say) is a warning, never a failed boot: what is left is the next
# runner's sweep.
function Remove-StaleRequests([string]$Root) {
    try {
        if (-not (Test-Path -LiteralPath $Root)) { return }
        foreach ($leftover in [IO.Directory]::GetFileSystemEntries($Root)) {
            try {
                Remove-Request $leftover -Once
                if ([IO.File]::Exists($leftover) -or [IO.Directory]::Exists($leftover)) { Send-Line "DEVVM-WARNING=a leftover is still there after one try: $leftover" }
            }
            catch {
                Send-Line "DEVVM-WARNING=a leftover could not be removed: $leftover`: $($_.Exception.Message)"
            }
        }
    }
    catch {
        Send-Line "DEVVM-WARNING=the leftovers could not be listed: $($_.Exception.Message)"
    }
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
    Remove-StaleRequests $root

    # The disk may appear after the boot has started.
    while (-not ($disk = Find-VolumeRoot 'CIDATA')) { Start-Sleep -Milliseconds 200 }

    # A boot that renames restarts and ends here: the seed, the boot id and sshd are the next boot's, which has the name.
    $restarting = Set-ComputerNameFromSeed $disk $renamedFile
    if ($restarting -isnot [bool]) { throw "Set-ComputerNameFromSeed answered $restarting, not a bool" }
    if ($restarting) { exit 0 }

    New-Item -ItemType Directory -Force -Path $ssh | Out-Null
    foreach ($name in $seed.Keys) {
        $target = Join-Path $ssh $seed[$name]
        Write-Whole $target ([IO.File]::ReadAllBytes((Join-Path $disk $name)))
        Send-Line "DEVVM-SEED-WRITTEN=$name"
    }
    # This boot's id, before sshd starts, so that any command that reaches the guest reads it.
    $bytes = [byte[]]::new(16)
    [Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
    Write-Whole $bootIdFile ([Text.Encoding]::ASCII.GetBytes([Convert]::ToHexString($bytes).ToLowerInvariant())) @('S-1-5-32-545')

    # C: as large as the disk. No partition follows it, so it can grow; a C: that is as large as the disk needs nothing.
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
