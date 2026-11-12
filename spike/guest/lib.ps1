# Spike only (never merged). Shared by the bootstrap, the hook and the stage scripts (PowerShell 5.1).
# Every `DEVVM-...` line goes to COM1 (the host's serial log, UTF-8) and to a log file that every account can write:
# $env:DEVVM_LOG, or C:\Users\Public\devvm-log.txt. Send-Line never throws: the user's logon task is not an administrator.

function Get-SerialMutex {
    # Created by whichever process comes first (SYSTEM or the user), so it grants everyone access.
    $created = $false
    $security = New-Object System.Security.AccessControl.MutexSecurity
    $everyone = New-Object System.Security.Principal.SecurityIdentifier 'S-1-1-0'
    $security.AddAccessRule((New-Object System.Security.AccessControl.MutexAccessRule($everyone, 'FullControl', 'Allow')))
    [System.Threading.Mutex]::new($false, 'Global\devvm-serial-acl', [ref]$created, $security)
}

function Send-Line {
    param([Parameter(Mandatory)][string]$Text)
    $line = ($Text -replace '\s*[\r\n]+\s*', ' ').Trim()
    # UTC ticks of the send: the serial log has no times of its own.
    $line = "$line @$([DateTime]::UtcNow.Ticks)"
    $log = if ($env:DEVVM_LOG) { $env:DEVVM_LOG } else { 'C:\Users\Public\devvm-log.txt' }
    try { Add-Content -LiteralPath $log -Value $line -Encoding UTF8 } catch { }

    # Two processes (the boot task and the logon task) may write at once: one at a time.
    $mutex = $null
    $held = $false
    try {
        $mutex = Get-SerialMutex
        try { [void]$mutex.WaitOne(); $held = $true } catch [System.Threading.AbandonedMutexException] { $held = $true }
    } catch {
        try { Add-Content -LiteralPath $log -Value "MUTEX-ERROR $($_.Exception.Message)" } catch { }
    }
    try {
        $com = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $com.Encoding = [System.Text.Encoding]::UTF8
        $com.Open()
        try { $com.Write($line + "`n") } finally { $com.Close() }
    } catch {
        try { Add-Content -LiteralPath $log -Value "SERIAL-ERROR $($_.Exception.Message)" } catch { }
    } finally {
        if ($held) { $mutex.ReleaseMutex() }
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
