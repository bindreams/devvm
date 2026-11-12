# Spike only (never merged). Stage `wedge`, boot task (SYSTEM): one variant per boot, named in variant.txt. The variants
# isolate the hang of a command with 1 MiB on stdin that sshd, the broker and QEMU's forward were all candidates for
# (run 43260908167): the host (`wedge_driver.py`) runs the same payloads in each.
#   nobroker   sshd's DefaultShell is pwsh itself: sshd -> pwsh -> emit, no broker
#   medium     sshd -> broker shim -> Medium agent -> pwsh -> emit
#   elevated   the same through the elevated agent (the host switches the agent first)
#   local      no sshd in the path: stage-user.ps1 runs the shim itself with pipes, from the user's own session
# sshd logs at DEBUG3 here (its own account of every channel read and write), and the watcher keeps the last part of it.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
. (Join-Path $Root 'ssh-common.ps1')

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'probe.ps1'), (Join-Path $Root 'watch.ps1'), (Join-Path $Root 'killagent.ps1') -Destination C:\ProgramData\devvm -Force
. C:\ProgramData\devvm\lib.ps1  # the copy sends the time with every line

$variant = (Get-Content -Raw -LiteralPath (Join-Path $Root 'variant.txt')).Trim()
Send-Line "DEVVM-WEDGE-VARIANT=$variant"

$bin = 'C:\ProgramData\devvm\m5\bin'
Step 'HELPERS' {
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'm5helper.cs')) -OutputType ConsoleApplication -OutputAssembly "$bin\argv.exe"
    Copy-Item -LiteralPath "$bin\argv.exe" -Destination "$bin\emit.exe" -Force
    Get-ChildItem $bin | Select-Object Name, Length | Out-String
}
Step 'BROKER-APPLY' { Invoke-BrokerApply $Root }
Step 'AGENT-TASKS' { Register-AgentTask 'devvm-agent-elevated' 'Highest'; Register-AgentTask 'devvm-agent-medium' 'Limited' }
if ($variant -eq 'nobroker') {
    # What `apply` set is the broker as the shell; sshd reads it for every session. pwsh runs the command itself.
    Step 'NOBROKER' {
        $key = 'HKLM:\SOFTWARE\OpenSSH'
        Set-ItemProperty -LiteralPath $key -Name DefaultShell -Value 'C:\Program Files\PowerShell\7\pwsh.exe'
        Get-ItemProperty -LiteralPath $key | Select-Object DefaultShell, DefaultShellCommandOption, DefaultShellEscapeArguments | Out-String
    }
}
Step 'SEED' { Invoke-Seed $Root }
Step 'SSHD-START' { Invoke-SshdStart 'DEBUG3' }
Step 'WATCHER' { Start-Watcher }
Send-Line 'DEVVM-SSH-STARTED'
