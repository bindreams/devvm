# Spike only (never merged). The guest's own view while the host drives SSH: runs as the SYSTEM task `devvm-watch`
# (started by ssh-common.ps1's Start-Watcher), and every 10 s sends one `DEVVM-WATCH` line to the serial log, so that a
# moment when sshd stops answering still has the guest's state next to it. PowerShell 5.1. Needs lib.ps1 (Send-Line).
#   procs   count per name of the processes that carry a command (sshd, sshd-session, sftp-server, ssh-broker, pwsh, ...)
#   tcp22   TCP connections on port 22 by state
#   ssh     the sshd service state and the handle and thread counts of its processes
#   mem     free physical memory in MB
# A line `DEVVM-WATCH-PROCS` lists the processes with parent, session, age and CPU seconds whenever that list changed.
# A line `DEVVM-WATCH-IO` (every cycle) gives per process the bytes it has read and written (Win32_Process transfer
# counts: every file, pipe and socket read or write), its thread count and what its threads wait for: where a stream
# stopped is the process whose reads exceed its writes by the stuck bytes.
# `DEVVM-WATCH-SSHD` is the last part of the new text in sshd's own log (SyslogFacility LOCAL0, ssh-common.ps1);
# `DEVVM-WATCH-LOG` is new text in the broker's logs.
. C:\ProgramData\devvm\lib.ps1
$names = 'sshd', 'sshd-session', 'sftp-server', 'ssh-broker', 'pwsh', 'powershell', 'emit', 'argv', 'conhost', 'OpenConsole'
$lastList = ''
$sshdFrom = 0
$offsets = @{}
$began = Get-Date

while ($true) {
    try {
        $now = Get-Date
        $procs = @(Get-Process -Name $names -ErrorAction SilentlyContinue)
        $counts = ($procs | Group-Object Name | Sort-Object Name | ForEach-Object { "$($_.Name):$($_.Count)" }) -join ','
        $tcp = (Get-NetTCPConnection -LocalPort 22 -ErrorAction SilentlyContinue | Group-Object State | ForEach-Object { "$($_.Name):$($_.Count)" }) -join ','
        $sshd = ($procs | Where-Object { $_.Name -like 'sshd*' } | ForEach-Object { "$($_.Id):h$($_.HandleCount):t$($_.Threads.Count)" }) -join ','
        $service = (Get-Service sshd -ErrorAction SilentlyContinue).Status
        $mem = [int]((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory / 1024)
        Send-Line "DEVVM-WATCH t=$($now.ToUniversalTime().ToString('HH:mm:ss')) up=$([int]($now - $began).TotalSeconds) mem=$mem service=$service ssh=$sshd tcp22=$tcp procs=$counts"

        $detail = Get-CimInstance Win32_Process -Filter "Name='ssh-broker.exe' OR Name='pwsh.exe' OR Name='emit.exe' OR Name='sftp-server.exe' OR Name='sshd.exe' OR Name='sshd-session.exe'" |
            Sort-Object ProcessId | ForEach-Object {
                $cpu = [int](Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue).CPU
                "$($_.Name)#$($_.ProcessId)<-$($_.ParentProcessId) s$($_.SessionId) cpu$cpu"
            }
        $list = $detail -join ' ; '
        $stable = ($detail | ForEach-Object { ($_ -replace ' cpu\d+', '') }) -join ' ; '
        if ($stable -ne $lastList) {
            $lastList = $stable
            Send-Line "DEVVM-WATCH-PROCS $list"
        }

        $io = Get-CimInstance Win32_Process -Filter "Name='ssh-broker.exe' OR Name='pwsh.exe' OR Name='emit.exe' OR Name='sshd.exe' OR Name='sshd-session.exe'" |
            Sort-Object ProcessId | ForEach-Object {
                $threads = @((Get-Process -Id $_.ProcessId -ErrorAction SilentlyContinue).Threads)
                $waits = ($threads | ForEach-Object { if ($_.ThreadState -eq 'Wait') { "W:$($_.WaitReason)" } else { "$($_.ThreadState)" } } | Group-Object | Sort-Object Name | ForEach-Object { "$($_.Name)*$($_.Count)" }) -join '+'
                "$($_.Name)#$($_.ProcessId) r=$($_.ReadTransferCount) w=$($_.WriteTransferCount) t=$($threads.Count) $waits"
            }
        Send-Line "DEVVM-WATCH-IO $($io -join ' ; ')"

        # sshd's own log: what is new since the last look, its last 6000 bytes (the lines just before a stall matter most).
        $sshdLog = 'C:\ProgramData\ssh\logs\sshd.log'
        if (Test-Path -LiteralPath $sshdLog) {
            $length = (Get-Item -LiteralPath $sshdLog).Length
            if ($length -gt $sshdFrom) {
                $stream = [IO.File]::Open($sshdLog, 'Open', 'Read', 'ReadWrite')
                try {
                    $start = [Math]::Max($sshdFrom, $length - 6000)
                    [void]$stream.Seek($start, 'Begin')
                    $bytes = New-Object byte[] ([int]($length - $start))
                    $read = $stream.Read($bytes, 0, $bytes.Length)
                    Send-Line "DEVVM-WATCH-SSHD new=$($length - $sshdFrom) $([Text.Encoding]::UTF8.GetString($bytes, 0, $read))"
                    $sshdFrom = $length
                } finally { $stream.Close() }
            }
        }

        foreach ($file in Get-ChildItem -LiteralPath 'C:\Users\user\AppData\Local\ssh-broker\logs' -File -ErrorAction SilentlyContinue) {
            $from = if ($offsets.ContainsKey($file.FullName)) { $offsets[$file.FullName] } else { 0 }
            if ($file.Length -gt $from) {
                $stream = [IO.File]::Open($file.FullName, 'Open', 'Read', 'ReadWrite')
                try {
                    [void]$stream.Seek($from, 'Begin')
                    $bytes = New-Object byte[] ([int][Math]::Min($file.Length - $from, 3000))
                    $read = $stream.Read($bytes, 0, $bytes.Length)
                    $offsets[$file.FullName] = $from + $read
                    Send-Line "DEVVM-WATCH-LOG $($file.Name) $([Text.Encoding]::UTF8.GetString($bytes, 0, $read))"
                } finally { $stream.Close() }
            }
        }
    } catch {
        Send-Line "DEVVM-WATCH-ERROR $($_ | Out-String)"
    }
    Start-Sleep -Seconds 10
}
