# Spike only (never merged). Shared by the bootstrap, the hook and the stage scripts (PowerShell 5.1).
# Every `DEVVM-...` line goes to COM1 (the host's serial log) and to C:\ProgramData\devvm\log.txt.

$script:LogPath = 'C:\ProgramData\devvm\log.txt'

function Send-Line {
    param([Parameter(Mandatory)][string]$Text)
    $line = ($Text -replace '\s*[\r\n]+\s*', ' ').Trim()
    Add-Content -LiteralPath $script:LogPath -Value $line
    # Two processes (the boot task and the logon task) may write at once: one at a time.
    $mutex = New-Object System.Threading.Mutex($false, 'Global\devvm-serial')
    [void]$mutex.WaitOne()
    try {
        $com = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $com.Open()
        try { $com.Write($line + "`n") } finally { $com.Close() }
    } catch {
        Add-Content -LiteralPath $script:LogPath -Value "SERIAL-ERROR $($_.Exception.Message)"
    } finally {
        $mutex.ReleaseMutex()
    }
}

# Runs a script block and sends its output as DEVVM-<Name>=<text>, never throwing.
function Send-Probe {
    param([Parameter(Mandatory)][string]$Name, [Parameter(Mandatory)][scriptblock]$Block)
    try {
        $text = (& $Block 2>&1 | Out-String)
    } catch {
        $text = "ERROR $($_ | Out-String)"
    }
    Send-Line "DEVVM-$Name=$text"
}

function Get-IntegritySid {
    $groups = whoami /groups /fo csv | ConvertFrom-Csv
    ($groups | Where-Object { $_.SID -like 'S-1-16-*' } | Select-Object -First 1).SID
}

function Get-SessionId {
    [System.Diagnostics.Process]::GetCurrentProcess().SessionId
}
