# Spike only (never merged). The guest side of E1 and E3 of the wedge root-cause run: runs as the SYSTEM task
# `devvm-hangdump` (ssh-common.ps1 Start-HangDump), PowerShell 5.1. Needs lib.ps1 (Send-Line).
#
# A transfer has stalled when `emit.exe` is alive and no sshd process's bytes read or written (nor emit's) changed for
# $StallSeconds: a normal 1 MiB run takes 0.5 to 2 s, and every hang seen had all counters still for its whole 120 s.
# Then, once per stalled episode (`DEVVM-HANGDUMP-TRIGGER <n>`): `pktmon stop` and its text (E3, with -Pkt; the next capture
# starts at once), `procdump` of every sshd, sshd-session, emit and pwsh process (E1), the process list with thread states, and
# sshd's own logs; all of it is PUT to the host's receiver ($Upload, wedge_driver.py) and `DEVVM-HANGDUMP-DONE <n>` goes to
# the serial log. An episode ends when a counter moves or emit.exe is gone, so a slow pass that recovers has its own
# dump and a later hang another.
# After the first dump the listener is probed every 10 s: a connection to 127.0.0.1:22 that gets no banner within
# $BannerSeconds is the second symptom (sshd accepts and serves nobody); the processes are dumped again as
# `listener-wedged-*` and `DEVVM-HANGDUMP-LISTENER-DONE` ends the script. The banner wait is the failure bound for an event
# that may never come.
param(
    [string]$Procdump = 'C:\ProgramData\devvm\procdump64.exe',
    [string]$Upload = 'http://10.0.2.2:8765',
    [switch]$Pkt,
    [int]$StallSeconds = 20,
    [int]$BannerSeconds = 15
)

$dir = 'C:\ProgramData\devvm\hang'
$sent = @{}

# One text per look at the processes that carry the transfer. $null while no emit.exe lives. The counters are
# Win32_Process transfer counts (every file, pipe and socket read or write), as in watch.ps1.
function Get-Signature {
    $procs = @(Get-CimInstance Win32_Process -Filter "Name='emit.exe' OR Name='sshd.exe' OR Name='sshd-session.exe'")
    if (-not ($procs | Where-Object { $_.Name -eq 'emit.exe' })) { return $null }
    ($procs | Sort-Object ProcessId | ForEach-Object { "$($_.Name)#$($_.ProcessId):$($_.ReadTransferCount):$($_.WriteTransferCount)" }) -join ';'
}

# $State carries what the last look saw: Signature and Since. Returns $true once the signature has not changed for
# $Seconds while a transfer lives. A pure function of its arguments, so that it can be replayed on captured counters.
function Step-Stall([hashtable]$State, $Signature, [datetime]$Now, [int]$Seconds) {
    if ($null -eq $Signature) { $State.Signature = $null; $State.Since = $null; return $false }
    if ($Signature -ne $State.Signature) { $State.Signature = $Signature; $State.Since = $Now; return $false }
    return (($Now - $State.Since).TotalSeconds -ge $Seconds)
}

function Save-Text([string]$Name, [scriptblock]$Block) {
    try { (& $Block 2>&1 | Out-String) | Set-Content -LiteralPath (Join-Path $dir $Name) -Encoding UTF8 }
    catch { Send-Line "DEVVM-HANGDUMP-ERROR $Name $($_ | Out-String)" }
}

function Save-Dumps([string]$Label) {
    Save-Text "$Label-processes.txt" {
        Get-CimInstance Win32_Process -Filter "Name='emit.exe' OR Name='sshd.exe' OR Name='sshd-session.exe' OR Name='pwsh.exe' OR Name='ssh-broker.exe'" |
            Sort-Object ProcessId | ForEach-Object {
                $threads = @((Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue).Threads)
                "$($_.Name)#$($_.ProcessId)<-$($_.ParentProcessId) s$($_.SessionId) r=$($_.ReadTransferCount) w=$($_.WriteTransferCount) $($_.CommandLine)"
                foreach ($thread in $threads) { "    tid $($thread.Id) $($thread.ThreadState) $($thread.WaitReason) start $($thread.StartAddress)" }
            }
        Get-NetTCPConnection -LocalPort 22 -ErrorAction SilentlyContinue | Select-Object LocalAddress, RemoteAddress, State, OwningProcess | Format-Table | Out-String
    }
    # The children too (run 43310480365 read only sshd's side): emit.exe in full (its main thread's wait and its handles),
    # pwsh.exe as a minidump (native thread stacks; a full dump of a PowerShell process is hundreds of MB).
    foreach ($process in @(Get-Process -Name 'sshd', 'sshd-session', 'emit', 'pwsh' -ErrorAction SilentlyContinue)) {
        $file = Join-Path $dir "$Label-$($process.Name)-$($process.Id).dmp"
        $kind = if ($process.Name -eq 'pwsh') { '-mm' } else { '-ma' }
        $output = & $Procdump -accepteula $kind $process.Id $file 2>&1 | Out-String
        Send-Line "DEVVM-HANGDUMP-PROCDUMP $Label $($process.Name)#$($process.Id) exit=$LASTEXITCODE $output"
    }
    foreach ($log in @(Get-ChildItem -LiteralPath 'C:\ProgramData\ssh\logs' -Filter '*.log' -File -ErrorAction SilentlyContinue)) {
        Copy-Item -LiteralPath $log.FullName -Destination (Join-Path $dir "$Label-log-$($log.Name)") -Force
    }
}

# Every file in $dir not yet sent, to the host. A failure is reported and the file stays here.
function Send-Files {
    [Net.ServicePointManager]::Expect100Continue = $false
    foreach ($file in @(Get-ChildItem -LiteralPath $dir -File)) {
        if ($sent.ContainsKey($file.Name)) { continue }
        try {
            Invoke-WebRequest -UseBasicParsing -Method Put -InFile $file.FullName -Uri "$Upload/$($file.Name)" -ContentType 'application/octet-stream' | Out-Null
            $sent[$file.Name] = $true
            Send-Line "DEVVM-HANGDUMP-UPLOADED $($file.Name) $($file.Length)"
        } catch {
            Send-Line "DEVVM-HANGDUMP-UPLOAD-FAILED $($file.Name) $($file.Length) $($_ | Out-String)"
        }
    }
}

# $true when a connection to port 22 gets sshd's banner line.
function Test-Banner {
    $client = New-Object Net.Sockets.TcpClient
    try {
        $client.Connect('127.0.0.1', 22)
        $stream = $client.GetStream()
        $stream.ReadTimeout = $BannerSeconds * 1000
        $buffer = New-Object byte[] 4
        return (($stream.Read($buffer, 0, 4) -eq 4) -and ([Text.Encoding]::ASCII.GetString($buffer) -eq 'SSH-'))
    } catch {
        return $false
    } finally {
        $client.Close()
    }
}

# pktmon: one capture file per stalled episode, `pktmon-<n>.etl`. Stopping it writes the file; the next capture starts at
# once, so that a later episode (a real hang after a slow pass) has its own.
function Start-Capture([int]$Episode) {
    $file = Join-Path $dir "pktmon-$Episode.etl"
    Save-Text "pktmon-start-$Episode.txt" {
        pktmon start --capture --comp nics --pkt-size 0 --file-name $file --file-size 512
        pktmon status
    }
}

function Stop-Capture([int]$Episode) {
    $file = Join-Path $dir "pktmon-$Episode.etl"
    Save-Text "pktmon-stop-$Episode.txt" { pktmon stop }
    Save-Text "pktmon-counters-$Episode.txt" { pktmon counters }
    if (Test-Path -LiteralPath $file) {
        Save-Text "pktmon-etl2txt-$Episode.txt" { pktmon etl2txt $file --out (Join-Path $dir "pktmon-$Episode.txt") --timestamp }
        Save-Text "pktmon-stats-$Episode.txt" { pktmon etl2txt $file --out (Join-Path $dir "pktmon-stats-$Episode.txt") --stats }
    } else {
        Send-Line "DEVVM-HANGDUMP-ERROR $file does not exist"
    }
}

function Main {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $episodes = 0
    if ($Pkt) {
        Save-Text 'pktmon-filter.txt' { pktmon filter add devvm22 -p 22 }
        Start-Capture 1
    }
    $state = @{ Signature = $null; Since = $null }
    $dumped = $false  # the stalled episode in progress has its dump
    $nextProbe = Get-Date
    while ($true) {
        if (-not (Step-Stall $state (Get-Signature) (Get-Date) $StallSeconds)) {
            $dumped = $false
        } elseif (-not $dumped) {
            $dumped = $true
            $episodes++
            Send-Line "DEVVM-HANGDUMP-TRIGGER $episodes $($state.Signature)"
            if ($Pkt) { Stop-Capture $episodes; Start-Capture ($episodes + 1) }
            Save-Dumps "stall$episodes"
            Send-Files
            Send-Line "DEVVM-HANGDUMP-DONE $episodes"
        }
        # Once a stall was dumped the listener is looked at too: no banner is the second symptom.
        if ($episodes -gt 0 -and (Get-Date) -ge $nextProbe) {
            if (-not (Test-Banner)) {
                Send-Line 'DEVVM-HANGDUMP-LISTENER-NO-BANNER'
                Save-Dumps 'listener-wedged'
                Send-Files
                Send-Line 'DEVVM-HANGDUMP-LISTENER-DONE'
                return
            }
            $nextProbe = (Get-Date).AddSeconds(10)
        }
        Start-Sleep -Seconds 2  # pacing between looks; the stall is the condition
    }
}

# Dot-sourcing (the replay of Step-Stall on captured counters) must not start the watch.
if ($MyInvocation.InvocationName -ne '.') {
    . C:\ProgramData\devvm\lib.ps1
    Main
}
