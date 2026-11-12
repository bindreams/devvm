# Spike only (never merged). Stage `restart`, boot task (SYSTEM), on every boot of the stage's launches:
#   first boot:  C: into the larger disk, `ssh-broker apply`, then the seed and sshd (as the ssh stage did).
#   later boots: sshd starts with the service manager (the first boot sets it to Automatic), before the logon has made
#                the agent: M4's window. Run 43260908167 had sshd started by this task, 21 s after the agent, so no
#                window existed. The seed and `Start-Service` still run, for a boot where the files came back empty.
#                What ran on the first boot is recorded by the marker file.
# Variant `eb` also registers the E-B helpers on the first boot only (M6: they must survive every later boot).
# The host side (`restart_driver.py`) drives the launches: restart, shutdown, in-place restart, QMP reset.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
. (Join-Path $Root 'ssh-common.ps1')

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'probe.ps1'), (Join-Path $Root 'watch.ps1') -Destination C:\ProgramData\devvm -Force
. C:\ProgramData\devvm\lib.ps1  # the copy sends the time with every line

$marker = 'C:\ProgramData\devvm\restart-first-boot-done'
if (-not (Test-Path -LiteralPath $marker)) {
    Send-Line 'DEVVM-BOOT=first'
    Step 'BASE-SSH-DIR' { Get-ChildItem C:\ProgramData\ssh -ErrorAction SilentlyContinue | Select-Object Name, Length | Out-String }
    Step 'EXTEND-C' { Invoke-ExtendC }
    Step 'BROKER-APPLY' { Invoke-BrokerApply $Root }
    Step 'SSHD-AUTOMATIC' { Set-Service -Name sshd -StartupType Automatic; (Get-Service sshd).StartType.ToString() }
    # Variant `eb` (M6): the elevated helpers are registered here once; later boots must find them as they are.
    if ((Get-Content -Raw -LiteralPath (Join-Path $Root 'variant.txt')).Trim() -eq 'eb') {
        Step 'EB-HELPER' { Register-EbHelper $Root }
        Step 'EB-PARALLEL-HELPER' { Register-EbParallelHelper $Root }
    }
    Set-Content -LiteralPath $marker -Value (Get-Date -Format o)
} else {
    Send-Line 'DEVVM-BOOT=later'
}
Step 'SEED' { Invoke-Seed $Root }
Step 'SSHD-START' { Invoke-SshdStart }
Send-Line 'DEVVM-SSH-STARTED'
Step 'WATCHER' { Start-Watcher }
Step 'AGENT-TASK' { Get-ScheduledTask | Where-Object { $_.TaskName -like '*ssh-broker*' -or $_.TaskPath -like '*ssh-broker*' } | Select-Object TaskPath, TaskName, State | Out-String }
