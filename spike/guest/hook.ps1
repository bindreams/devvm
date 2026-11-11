# Spike only (never merged). Run by the boot task (SYSTEM, -Mode system) and the logon task (user, -Mode user).
# Runs `stage-<Mode>.ps1` from the volume labelled CIDATA, so a stage is a different USB disk, not a new install.
param([Parameter(Mandatory)][ValidateSet('system', 'user')][string]$Mode)

$ErrorActionPreference = 'Stop'
. C:\ProgramData\devvm\lib.ps1

# A boot without a CIDATA disk waits here; nothing else needs this task then.
while (-not (Get-Volume -FileSystemLabel CIDATA -ErrorAction SilentlyContinue)) {
    Start-Sleep -Seconds 1
}
$root = '{0}:\' -f (Get-Volume -FileSystemLabel CIDATA).DriveLetter
$script = Join-Path $root "stage-$Mode.ps1"
if (-not (Test-Path -LiteralPath $script)) {
    Send-Line "DEVVM-STAGE-NONE=$Mode"
    exit 0
}

Send-Line "DEVVM-STAGE-BEGIN=$Mode"
try {
    & $script -Root $root
} catch {
    Send-Line "DEVVM-STAGE-FAILED=$Mode $($_ | Out-String)"
}
Send-Line "DEVVM-STAGE-END=$Mode"
