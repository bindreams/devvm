# Shared by the first-logon script (Windows PowerShell 5.1, as `user`) and the boot task (PowerShell 7, as SYSTEM).
# Every DEVVM-... marker goes to COM1, the host's serial log, as UTF-8 on one line that
# starts with a line end (a serial terminal may write escape bytes first), and to a log file anyone can write.

$script:DevvmLog = Join-Path @@DEVVM_DIR@@ 'log.txt'

# Created by whichever process comes first, so it grants Everyone access: a SYSTEM process's mutex is otherwise closed to
# `user`.
function Get-SerialMutex {
    $created = $false
    $security = New-Object System.Security.AccessControl.MutexSecurity
    $everyone = New-Object System.Security.Principal.SecurityIdentifier 'S-1-1-0'
    $security.AddAccessRule((New-Object System.Security.AccessControl.MutexAccessRule($everyone, 'FullControl', 'Allow')))
    $name = 'Global\devvm-serial'
    if ($PSVersionTable.PSEdition -eq 'Core') {
        [System.Threading.MutexAcl]::Create($false, $name, [ref]$created, $security)
    } else {
        [System.Threading.Mutex]::new($false, $name, [ref]$created, $security)
    }
}

# Never throws: a marker that cannot be sent must not end the script that sends it. A mutex that cannot be made leaves
# the write unlocked; the boot task and the first-logon script never run at the same time.
function Send-Line {
    param([Parameter(Mandatory)][string]$Text)
    $line = ($Text -replace '\s*[\r\n]+\s*', ' ').Trim()
    try { Add-Content -LiteralPath $script:DevvmLog -Value $line -Encoding UTF8 } catch { }
    $mutex = $null
    $held = $false
    try {
        $mutex = Get-SerialMutex
        try { [void]$mutex.WaitOne(); $held = $true } catch [System.Threading.AbandonedMutexException] { $held = $true }
    } catch {
        try { Add-Content -LiteralPath $script:DevvmLog -Value "MUTEX-ERROR $($_.Exception.Message)" } catch { }
    }
    try {
        $com = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $com.Encoding = [System.Text.Encoding]::UTF8
        $com.Open()
        try { $com.Write("`n" + $line + "`n") } finally { $com.Close() }
    } catch {
        try { Add-Content -LiteralPath $script:DevvmLog -Value "SERIAL-ERROR $($_.Exception.Message)" } catch { }
    } finally {
        if ($held) { $mutex.ReleaseMutex() }
    }
}

# The root of the volume with this label, or nothing. DriveInfo, not the storage cmdlets: the user's token cannot query
# storage through CIM.
function Find-VolumeRoot([string]$Label) {
    foreach ($drive in [System.IO.DriveInfo]::GetDrives()) {
        if ($drive.IsReady -and $drive.VolumeLabel -eq $Label) { return $drive.RootDirectory.FullName }
    }
}

# Fails the script when the last native command did not end with status 0.
function Assert-Exit([string]$What) {
    if ($LASTEXITCODE -ne 0) { throw "$What ended with status $LASTEXITCODE" }
}
