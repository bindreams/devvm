# Spike only (never merged). Stage `wedge`, boot task (SYSTEM): one variant per boot, named in variant.txt. The variants
# isolate the hang of a command with 1 MiB on stdin that sshd, the broker and QEMU's forward were all candidates for
# (run 43260908167): the host (`wedge_driver.py`) runs the same payloads in each.
#   nobroker   sshd's DefaultShell is pwsh itself: sshd -> pwsh -> emit, no broker
#   medium     sshd -> broker shim -> Medium agent -> pwsh -> emit
#   elevated   the same through the elevated agent (the host switches the agent first)
#   local      no sshd in the path: stage-user.ps1 runs the shim itself with pipes, from the user's own session
# The variant is `<path>` plus dot-separated options (variant.txt, e.g. `medium.b95`):
#   d1 | d3   sshd logs at DEBUG1 | DEBUG3 to C:\ProgramData\ssh\logs\<program>.log (sshd.log for the listener,
#             sshd-session.log for the sessions), and the watcher keeps the last part of each
#             (no option: sshd's default log level, the setting every earlier hang was seen with)
#   dump      hangdump.ps1: on a stalled transfer, procdump -ma of every sshd process and the logs, sent to the host (E1)
#   pkt       with `dump`: a pktmon capture of port 22 per stalled episode, written and converted by hangdump.ps1 (E3)
#   eof       the host client sends the stdin EOF only after the whole echo came back (E2; the host's option, nothing here)
#   b95 | p98 | p100 | inbox   a different sshd instead of the base's pinned 10.4.0.0p1-Preview (the wedge remedies)
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
. (Join-Path $Root 'ssh-common.ps1')

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'probe.ps1'), (Join-Path $Root 'watch.ps1'), (Join-Path $Root 'killagent.ps1') -Destination C:\ProgramData\devvm -Force
. C:\ProgramData\devvm\lib.ps1  # the copy sends the time with every line

$full = (Get-Content -Raw -LiteralPath (Join-Path $Root 'variant.txt')).Trim()
$tokens = $full.Split('.')
$variant = $tokens[0]
$sshdKey = $tokens | Where-Object { $_ -in 'b95', 'p98', 'p100', 'inbox' } | Select-Object -First 1
$logLevel = if ($tokens -contains 'd3') { 'DEBUG3' } elseif ($tokens -contains 'd1') { 'DEBUG1' } else { '' }
$dump = $tokens -contains 'dump'
$capture = $tokens -contains 'pkt'
Send-Line "DEVVM-WEDGE-VARIANT=$full path=$variant sshd=$sshdKey loglevel=$logLevel"

# The sshd to measure. Win32-OpenSSH zips from GitHub, pinned by sha256 (10.0.0.0p2's is the release's own digest; the
# others are the hash of the download on 2026-11-12, sizes equal to the release's); `inbox` is Windows' own optional
# feature. The base's pinned 10.4.0.0p1-Preview is uninstalled first; the zip goes straight into Program Files (a
# directory moved from a temp directory keeps the temp directory's ACL, which is what denied sftp-server.exe to the user).
$pins = @{
    b95  = @{ Tag = 'v9.5.0.0p1-Beta'; Sha = 'bd48fe985d400402c278c485db20e6a82bc4c7f7d8e0ef5a81128f523096530c' }
    p98  = @{ Tag = 'v9.8.3.0p2-Preview'; Sha = '0ca131f3a78f404dc819a6336606caec0db1663a692ccc3af1e90232706ada54' }
    p100 = @{ Tag = '10.0.0.0p2-Preview'; Sha = '23f50f3458c4c5d0b12217c6a5ddfde0137210a30fa870e98b29827f7b43aba5' }
}

function Uninstall-BaseSshd {
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File 'C:\Program Files\OpenSSH\uninstall-sshd.ps1' 2>&1 | Out-String
    Remove-Item -LiteralPath 'C:\Program Files\OpenSSH' -Recurse -Force
}

function Install-SshdVariant([string]$Key) {
    $ProgressPreference = 'SilentlyContinue'
    if ($Key -eq 'inbox') {
        Get-WindowsCapability -Online -Name 'OpenSSH*' | Select-Object Name, State | Out-String
        Uninstall-BaseSshd
        # Add-WindowsCapability downloads from Windows Update and may never return: 900 s is the failure bound.
        $job = Start-Job { Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' | Out-String }
        if (-not (Wait-Job $job -Timeout 900)) { throw 'Add-WindowsCapability OpenSSH.Server did not finish in 900 s' }
        Receive-Job $job
        Get-WindowsCapability -Online -Name 'OpenSSH*' | Select-Object Name, State | Out-String
        Set-Service -Name sshd -StartupType Manual
        New-NetFirewallRule -Name devvm-sshd -DisplayName 'devvm sshd' -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 -Profile Any -ErrorAction SilentlyContinue | Out-Null
    } else {
        $pin = $pins[$Key]
        $zip = Join-Path $env:TEMP "sshd-$Key.zip"
        Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/PowerShell/Win32-OpenSSH/releases/download/$($pin.Tag)/OpenSSH-Win64.zip" -OutFile $zip
        $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
        if ($got -ne $pin.Sha) { throw "sha256 of $($pin.Tag) is $got, expected $($pin.Sha)" }
        Uninstall-BaseSshd
        Expand-Archive -LiteralPath $zip -DestinationPath 'C:\Program Files' -Force
        Rename-Item -LiteralPath 'C:\Program Files\OpenSSH-Win64' -NewName 'OpenSSH'
        & powershell.exe -NoProfile -ExecutionPolicy Bypass -File 'C:\Program Files\OpenSSH\install-sshd.ps1' 2>&1 | Out-String
        Set-Service -Name sshd -StartupType Manual
    }
    $path = (Get-CimInstance Win32_Service -Filter "Name='sshd'").PathName -replace '^"?([^"]*\.exe).*', '$1'
    "sshd service binary: $path, file version $((Get-Item -LiteralPath $path).VersionInfo.FileVersion)"
    Get-ChildItem (Split-Path $path) -Filter 'sshd*' | Select-Object Name, Length | Out-String
}

if ($variant -eq 'echo') {
    # A TCP echo on port 22 behind the same QEMU forward: the network and the client without sshd, pipes or a shell. If this
    # stalls the way the others do, the stall is not sshd's.
    Step 'ECHO-SERVER' {
        $exe = 'C:\ProgramData\devvm\echo\devvm-echo.exe'
        New-Item -ItemType Directory -Force -Path (Split-Path $exe) | Out-Null
        Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'devvm-echo.cs')) -OutputType ConsoleApplication -OutputAssembly $exe
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        Register-ScheduledTask -TaskName 'devvm-echo' -Action (New-ScheduledTaskAction -Execute $exe) -Settings $settings -Force `
            -Principal (New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest) | Out-Null
        Start-ScheduledTask -TaskName 'devvm-echo'
        while (-not (Get-NetTCPConnection -LocalPort 22 -State Listen -ErrorAction SilentlyContinue)) { Start-Sleep -Milliseconds 200 }  # pacing; the listener is the condition
        'listening on 22'
    }
    Step 'WATCHER' { Start-Watcher }
    Send-Line 'DEVVM-ECHO-READY'
    return
}

$bin = 'C:\ProgramData\devvm\m5\bin'
Step 'HELPERS' {
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'm5helper.cs')) -OutputType ConsoleApplication -OutputAssembly "$bin\argv.exe"
    Copy-Item -LiteralPath "$bin\argv.exe" -Destination "$bin\emit.exe" -Force
    Get-ChildItem $bin | Select-Object Name, Length | Out-String
}
if ($sshdKey) { Step 'SSHD-VARIANT' { Install-SshdVariant $sshdKey } }
Step 'BROKER-APPLY' { Invoke-BrokerApply $Root }
Step 'AGENT-TASKS' { Register-AgentTask 'devvm-agent-elevated' 'Highest'; Register-AgentTask 'devvm-agent-medium' 'Limited' }
if ($variant -eq 'nobroker') {
    # What `apply` set is the broker as the shell; sshd reads it for every session. pwsh runs the command itself.
    Step 'NOBROKER' {
        $key = 'HKLM:\SOFTWARE\OpenSSH'
        Set-ItemProperty -LiteralPath $key -Name DefaultShell -Value 'C:\Program Files\PowerShell\7\pwsh.exe'
        Get-ItemProperty -LiteralPath $key | Select-Object DefaultShell, DefaultShellCommandOption, DefaultShellEscapeArguments | Out-String
    }
}
Step 'SEED' { Invoke-Seed $Root }
Step 'SSHD-START' { Invoke-SshdStart $logLevel }
Step 'WATCHER' { Start-Watcher }
if ($dump) { Step 'HANGDUMP' { Start-HangDump $Root $capture } }
Send-Line 'DEVVM-SSH-STARTED'
