# Spike only (never merged). Run by the boot task (SYSTEM, -Mode system) and the logon task (user, -Mode user).
# Runs `stage-<Mode>.ps1` from the volume labelled CIDATA, so a stage is a different USB disk, not a new install.
# Writes C:\Users\Public\devvm-<Mode>.log (what it sent) and devvm-done-<Mode> (when it ended, whatever the outcome).
param([Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Mode)

$ErrorActionPreference = 'Stop'
$env:DEVVM_LOG = "C:\Users\Public\devvm-$Mode.log"
. C:\ProgramData\devvm\lib.ps1

# DriveInfo, not Get-Volume: the user's token cannot query storage through CIM.
function Find-StageRoot {
    foreach ($drive in [System.IO.DriveInfo]::GetDrives()) {
        if ($drive.IsReady -and $drive.VolumeLabel -eq 'CIDATA') { return $drive.RootDirectory.FullName }
    }
}

Send-Line "DEVVM-STAGE-HOOK=$Mode as $([Security.Principal.WindowsIdentity]::GetCurrent().User.Value)"
try {
    # A boot without a CIDATA disk waits here; nothing else needs this task then.
    $root = $null
    while (-not ($root = Find-StageRoot)) {
        Start-Sleep -Seconds 1
    }
    $script = Join-Path $root "stage-$Mode.ps1"
    if (-not (Test-Path -LiteralPath $script)) {
        Send-Line "DEVVM-STAGE-NONE=$Mode"
    } else {
        Send-Line "DEVVM-STAGE-BEGIN=$Mode"
        try {
            & $script -Root $root
        } catch {
            Send-Line "DEVVM-STAGE-FAILED=$Mode $($_ | Out-String)"
        }
        Send-Line "DEVVM-STAGE-END=$Mode"
    }
} finally {
    Set-Content -LiteralPath "C:\Users\Public\devvm-done-$Mode" -Value (Get-Date -Format o)
}
