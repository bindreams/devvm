# Spike only (never merged). Stage `restart`, boot task (SYSTEM), on every boot of the stage's launches:
#   first boot:  C: into the larger disk, `ssh-broker apply`, then the seed and sshd (as the ssh stage did).
#   later boots: the seed and sshd first thing, before anything else, so that sshd answers before the logon has made
#                the agent (M4's window). What ran on the first boot is recorded by the marker file.
# The host side (`restart_driver.py`) drives the launches: restart, shutdown, in-place restart, QMP reset.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
. (Join-Path $Root 'ssh-common.ps1')

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'probe.ps1') -Destination C:\ProgramData\devvm -Force

$marker = 'C:\ProgramData\devvm\restart-first-boot-done'
if (-not (Test-Path -LiteralPath $marker)) {
    Send-Line 'DEVVM-BOOT=first'
    Step 'BASE-SSH-DIR' { Get-ChildItem C:\ProgramData\ssh -ErrorAction SilentlyContinue | Select-Object Name, Length | Out-String }
    Step 'EXTEND-C' { Invoke-ExtendC }
    Step 'BROKER-APPLY' { Invoke-BrokerApply $Root }
    Set-Content -LiteralPath $marker -Value (Get-Date -Format o)
} else {
    Send-Line 'DEVVM-BOOT=later'
}
Step 'SEED' { Invoke-Seed $Root }
Step 'SSHD-START' { Invoke-SshdStart }
Send-Line 'DEVVM-SSH-STARTED'
Step 'AGENT-TASK' { Get-ScheduledTask | Where-Object { $_.TaskName -like '*ssh-broker*' -or $_.TaskPath -like '*ssh-broker*' } | Select-Object TaskPath, TaskName, State | Out-String }
