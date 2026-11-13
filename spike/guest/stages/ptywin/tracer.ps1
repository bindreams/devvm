# Spike only (never merged). Runs as the SYSTEM task `devvm-tracer`, from every boot's stage script. PowerShell 5.1. Needs
# lib.ps1 (Send-Line). The guest's account of every process the pty exec request starts and ends, and of the files the
# host cannot read as the user:
#   C:\Users\Public\devvm-proc-events.txt   Security 4688 (process created: image, command line, parent, creator pid) and
#                                           4689 (process ended: exit status), and Application 1000/1001/1002 (crash
#                                           and error reports), every one since the boot, in order. Auditing is enabled by
#                                           the first boot (stage-system.ps1).
#   C:\Users\Public\devvm-sshd-logs\        a shared-read copy of each sshd log (C:\ProgramData\ssh\logs\*.log)
#   C:\Users\Public\devvm-tracer-beat.txt   UTC ticks of the start of the last finished cycle: what the host waits on to
#                                           know everything before a moment it has noted is in the files
# The lines of the processes that matter also go to the serial log (`DEVVM-PROC`).
. C:\ProgramData\devvm\lib.ps1

$events = 'C:\Users\Public\devvm-proc-events.txt'
$beat = 'C:\Users\Public\devvm-tracer-beat.txt'
$copies = 'C:\Users\Public\devvm-sshd-logs'
$matters = 'ssh|conhost|OpenConsole|WindowsTerminal|\\wt\.exe|pwsh|powershell|cmd\.exe|emit|devvm|WerFault|winwatch'

# One line per event; $Data holds the event's named fields. Pure, so that it can be replayed on captured text.
function Format-ProcEvent([int]$Id, [string]$Time, [hashtable]$Data) {
    switch ($Id) {
        4688 { "$Time 4688 CREATED new=$($Data.NewProcessName) pid=$($Data.NewProcessId) parent=$($Data.ParentProcessName) creator-pid=$($Data.ProcessId) elevation=$($Data.TokenElevationType) logon=$($Data.SubjectLogonId) cmd=$($Data.CommandLine)" }
        4689 { "$Time 4689 ENDED process=$($Data.ProcessName) pid=$($Data.ProcessId) status=$($Data.Status)" }
        default { "$Time $Id APPLICATION $($Data.Message)" }
    }
}

function Get-EventData($Entry) {
    $data = @{}
    $xml = [xml]$Entry.ToXml()
    foreach ($item in @($xml.Event.EventData.Data)) {
        if ($item.Name) { $data[$item.Name] = [string]$item.'#text' }
    }
    $data['Message'] = (($Entry.Message -replace '\s+', ' ')).Trim()
    $data
}

function Copy-Shared([string]$From, [string]$To) {
    $in = [IO.File]::Open($From, 'Open', 'Read', 'ReadWrite')
    try {
        $out = [IO.File]::Open($To, 'Create', 'Write', 'Read')
        try { $in.CopyTo($out) } finally { $out.Close() }
    } finally { $in.Close() }
}

# Run only when started as the task (a replay dot-sources this file for the functions above).
if ($MyInvocation.InvocationName -eq '.') { return }

New-Item -ItemType Directory -Force -Path $copies | Out-Null
$bootIso = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss.fffZ')
$last = @{ Security = 0L; Application = 0L }
$copied = @{}

while ($true) {
    $cycle = [DateTime]::UtcNow.Ticks
    try {
        $all = New-Object System.Collections.Generic.List[string]
        foreach ($log in 'Security', 'Application') {
            $ids = if ($log -eq 'Security') { 'EventID=4688 or EventID=4689' } else { 'EventID=1000 or EventID=1001 or EventID=1002' }
            $since = if ($last[$log] -gt 0) { "EventRecordID > $($last[$log])" } else { "TimeCreated[@SystemTime >= '$bootIso']" }
            foreach ($entry in @(Get-WinEvent -LogName $log -FilterXPath "*[System[($ids) and $since]]" -Oldest -ErrorAction SilentlyContinue)) {
                $last[$log] = [Math]::Max($last[$log], [long]$entry.RecordId)
                $all.Add((Format-ProcEvent $entry.Id $entry.TimeCreated.ToUniversalTime().ToString('o') (Get-EventData $entry)))
            }
        }
        if ($all.Count -gt 0) {
            Add-Content -LiteralPath $events -Value $all -Encoding UTF8
            $important = @($all | Where-Object { $_ -match $matters -or $_ -match 'APPLICATION' })
            if ($important.Count -gt 0) {
                $text = $important -join ' ;; '
                Send-Line "DEVVM-PROC $($text.Substring(0, [Math]::Min($text.Length, 14000)))"
            }
        }
        foreach ($file in Get-ChildItem -LiteralPath 'C:\ProgramData\ssh\logs' -Filter '*.log' -File -ErrorAction SilentlyContinue) {
            if ($copied[$file.Name] -ne $file.Length) {
                Copy-Shared $file.FullName (Join-Path $copies $file.Name)
                $copied[$file.Name] = $file.Length
            }
        }
    } catch {
        Send-Line "DEVVM-TRACER-ERROR $($_ | Out-String)"
    }
    Set-Content -LiteralPath $beat -Value $cycle -Encoding ascii
    Start-Sleep -Seconds 1  # the cycle's interval, not a wait for a condition
}
