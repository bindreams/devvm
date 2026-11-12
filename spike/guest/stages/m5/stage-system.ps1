# Spike only (never merged). Stage `m5`, boot task (SYSTEM): the programs and directories the M5 battery and M10 use,
# then `ssh-broker apply`, the seed and sshd. The host side (`m5_driver.py`) runs the battery over SSH.
#   C:\ProgramData\devvm\m5\bin\   emit.exe argv.exe -x.exe a=b.exe showargs.cmd (one compiled helper, four names)
#   C:\ProgramData\devvm\m5\work\  directories for the cwd tests
#   C:\Users\user\m10\             target.txt, subdir\, a file symlink and a directory symlink (SFTP, M10)
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
. (Join-Path $Root 'ssh-common.ps1')

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1'), (Join-Path $Root 'probe.ps1'), (Join-Path $Root 'watch.ps1'), (Join-Path $Root 'killagent.ps1') -Destination C:\ProgramData\devvm -Force
. C:\ProgramData\devvm\lib.ps1  # the copy sends the time with every line

$bin = 'C:\ProgramData\devvm\m5\bin'
$work = 'C:\ProgramData\devvm\m5\work'
Step 'HELPERS' {
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'm5helper.cs')) -OutputType ConsoleApplication -OutputAssembly "$bin\argv.exe"
    foreach ($name in 'emit.exe', '-x.exe', 'a=b.exe') { Copy-Item -LiteralPath "$bin\argv.exe" -Destination "$bin\$name" -Force }
    Copy-Item -LiteralPath (Join-Path $Root 'showargs.cmd') -Destination "$bin\showargs.cmd" -Force
    foreach ($dir in 'plain', 'dir with space', '[brackets]', "$([char]0xE9)$([char]0x4E16)", "it's") {
        [void][IO.Directory]::CreateDirectory((Join-Path $work $dir))
    }
    Get-ChildItem $bin | Select-Object Name, Length | Out-String
}
# M10: sftp-server.exe failed to start in the user's agent with "Access is denied" (run 43278227165). The base moved the
# extracted zip from SYSTEM's temp directory into Program Files, and a moved directory keeps its old ACL. The ACLs as they
# are, then reset to what Program Files gives (what an install would have), so that M10 itself can be measured.
Step 'OPENSSH-ACL-BEFORE' {
    foreach ($item in 'C:\Program Files\OpenSSH', 'C:\Program Files\OpenSSH\sftp-server.exe', 'C:\Program Files\OpenSSH\sshd.exe', 'C:\Program Files\PowerShell\7\pwsh.exe') {
        "== $item"
        icacls $item
    }
}
Step 'OPENSSH-ACL-RESET' {
    icacls 'C:\Program Files\OpenSSH' /reset /T /C | Select-Object -Last 2
    icacls 'C:\Program Files\OpenSSH\sftp-server.exe'
}
Step 'M10-FILES' {
    $m10 = 'C:\Users\user\m10'
    New-Item -ItemType Directory -Force -Path "$m10\subdir" | Out-Null
    Set-Content -LiteralPath "$m10\target.txt" -Value 'target'
    New-Item -ItemType SymbolicLink -Path "$m10\link-file" -Target "$m10\target.txt" | Out-Null
    New-Item -ItemType SymbolicLink -Path "$m10\link-dir" -Target "$m10\subdir" | Out-Null
    # The user's account must be able to write here: this directory was made by SYSTEM.
    icacls $m10 /grant 'user:(OI)(CI)F' /T | Out-Null
    Get-ChildItem $m10 | Select-Object Name, LinkType, Target | Out-String
}
Step 'BROKER-APPLY' { Invoke-BrokerApply $Root }
Step 'EB-HELPER' { Register-EbHelper $Root }
Step 'AGENT-TASKS' { Register-AgentTask 'devvm-agent-elevated' 'Highest'; Register-AgentTask 'devvm-agent-medium' 'Limited' }
Step 'SEED' { Invoke-Seed $Root }
Step 'SSHD-START' { Invoke-SshdStart }
Step 'WATCHER' { Start-Watcher }
Send-Line 'DEVVM-SSH-STARTED'
