# Spike only (never merged). Stage `uac` (variants `default` and `autoapprove`), boot task (SYSTEM).
# default:     UAC as installed (EnableLUA=1, ConsentPromptBehaviorAdmin=5). Registers the elevated helper task two ways
#              (M6 E-B: the default task security descriptor, and one that lets the user run it) for the user stage.
# autoapprove: ConsentPromptBehaviorAdmin=0 live, no reboot (M7).
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1

$variant = (Get-Content -LiteralPath (Join-Path $Root 'variant.txt') -ErrorAction SilentlyContinue | Select-Object -First 1)
Send-Line "DEVVM-UAC-VARIANT=$variant"
$policy = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System'
Send-Probe 'UAC-POLICY-BEFORE' { Get-ItemProperty $policy | Select-Object EnableLUA, ConsentPromptBehaviorAdmin, PromptOnSecureDesktop, FilterAdministratorToken | ConvertTo-Json -Compress }

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'helper-probe.ps1') -Destination C:\ProgramData\devvm -Force
Remove-Item -Path C:\Users\Public\devvm-ready-system, C:\Users\Public\devvm-*-out.txt* -ErrorAction SilentlyContinue

function Register-Helper([string]$Name, [string]$Out, $Sddl) {
    $sid = (New-Object Security.Principal.NTAccount('user')).Translate([Security.Principal.SecurityIdentifier]).Value
    $xml = @"
<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Principals><Principal id="Author"><UserId>$sid</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><MultipleInstancesPolicy>Parallel</MultipleInstancesPolicy></Settings>
  <Actions Context="Author"><Exec><Command>powershell.exe</Command><Arguments>-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File C:\ProgramData\devvm\helper-probe.ps1 -Out $Out</Arguments></Exec></Actions>
</Task>
"@
    $service = New-Object -ComObject Schedule.Service
    $service.Connect()
    $missing = [Type]::Missing
    $descriptor = if ($Sddl) { $Sddl } else { $missing }
    $service.GetFolder('\').RegisterTask($Name, $xml, 6, $missing, $missing, 3, $descriptor) | Out-Null
}

if ($variant -eq 'default') {
    $userSid = (New-Object Security.Principal.NTAccount('user')).Translate([Security.Principal.SecurityIdentifier]).Value
    Send-Probe 'HELPER-REGISTER-DEFAULT' { Register-Helper 'devvm-helper-default' 'C:\Users\Public\devvm-helper-default-out.txt' $null; 'registered' }
    Send-Probe 'HELPER-REGISTER-SDDL' {
        Register-Helper 'devvm-helper-sddl' 'C:\Users\Public\devvm-helper-sddl-out.txt' "D:(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFX;;;$userSid)"
        'registered'
    }
    Send-Probe 'HELPER-SDDL-READBACK' {
        $service = New-Object -ComObject Schedule.Service
        $service.Connect()
        foreach ($name in 'devvm-helper-default', 'devvm-helper-sddl') {
            "$name : $($service.GetFolder('\').GetTask($name).GetSecurityDescriptor(4))"
        }
    }
} elseif ($variant -eq 'autoapprove') {
    Set-ItemProperty -Path $policy -Name ConsentPromptBehaviorAdmin -Value 0 -Type DWord
}
Send-Probe 'UAC-POLICY-AFTER' { Get-ItemProperty $policy | Select-Object EnableLUA, ConsentPromptBehaviorAdmin, PromptOnSecureDesktop, FilterAdministratorToken | ConvertTo-Json -Compress }
Set-Content -LiteralPath C:\Users\Public\devvm-ready-system -Value (Get-Date -Format o)

# Until the user's part has ended; then the log it kept, so a failure on its side shows in the serial log.
while (-not (Test-Path -LiteralPath C:\Users\Public\devvm-done-user)) { Start-Sleep -Seconds 1 }
if (Test-Path -LiteralPath C:\Users\Public\devvm-user.log) {
    foreach ($line in Get-Content -LiteralPath C:\Users\Public\devvm-user.log -Encoding UTF8) { Send-Line "DEVVM-RELAY: $line" }
}
# A consent prompt left on the secure desktop would hold up the shutdown.
Get-Process consent -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
