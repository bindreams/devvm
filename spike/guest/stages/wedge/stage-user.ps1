# Spike only (never merged). Stage `wedge`, logon task (the user, Medium): the `local` variant. The same 1 MiB-on-stdin
# runs as the host makes over SSH, with no sshd and no network in the path: this script starts the program itself, with
# pipes, the way sshd starts the broker's shim. Phases, each with the three payloads (payload-*.ps1):
#   pwsh-direct      `pwsh -c <script>`: the shell alone (control)
#   broker-medium    `ssh-broker -c <script>` through the Medium agent
#   broker-elevated  the same through the elevated agent (the Medium agent is ended first)
# A run that does not end within 60 s is a HANG: the counts are the finding, the process tree is ended and the phase goes
# on with its next payload. Lines `DEVVM-WEDGE-LOCAL ...` go to the serial log; the last one is DEVVM-WEDGE-LOCAL-DONE.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1

if ((Get-Content -Raw -LiteralPath (Join-Path $Root 'variant.txt')).Trim() -ne 'local') { return }

# The number of runs per payload and phase is the sample size of the measurement, not a retry limit.
$samples = 20
$bound = 60000  # ms: the failure bound of one run (a run takes about a second)
$shim = 'C:\ProgramData\ssh-broker\ssh-broker.exe'
$pwsh = 'C:\Program Files\PowerShell\7\pwsh.exe'

Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'wedge-run.cs'))
$input1 = [Wedge]::Pattern(1)
$nothing = New-Object byte[] 0
$payloads = [ordered]@{}
foreach ($name in 'plain', 'c1', 'c2') {
    $payloads[$name] = Get-Content -Raw -LiteralPath (Join-Path $Root "payload-$name.ps1")
}

# What a command run through the shim sees of its parent: the agent (the relay worked) or the shim (it failed open).
$probeScript = @'
$parentId = (Get-CimInstance Win32_Process -Filter "ProcessId=$PID").ParentProcessId
$parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$parentId").CommandLine
"parent-is-agent=$($parent -match ' agent')"
"elevated=$(([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
"session=$([Diagnostics.Process]::GetCurrentProcess().SessionId)"
'@

# Waits (re-checking) until a command through the shim is run by an agent of the wanted kind.
function Wait-Agent([bool]$Elevated) {
    while ($true) {
        try {
            $r = [Wedge]::Run($shim, $probeScript, $nothing, $bound, $true)
            if ($r.Exited -and $r.Out -match 'parent-is-agent=True' -and $r.Out -match "elevated=$Elevated") {
                Send-Line "DEVVM-WEDGE-LOCAL agent ready (elevated=$Elevated): $($r.Out -replace '\s+', ' ')"
                return
            }
        } catch { Send-Line "DEVVM-WEDGE-LOCAL agent not ready: $($_.Exception.Message)" }
        Start-Sleep -Seconds 1  # pacing between probes
    }
}

function Measure-Phase([string]$Phase, [string]$Exe) {
    foreach ($name in $payloads.Keys) {
        for ($i = 1; $i -le $samples; $i++) {
            $r = [Wedge]::Run($Exe, $payloads[$name], $input1, $bound, $false)
            $ok = $r.Exited -and $r.ExitCode -eq 0 -and $r.Same -and $r.Received -eq $input1.Length
            Send-Line "DEVVM-WEDGE-LOCAL $Phase $name #$i $(if ($ok) { 'PASS' } elseif ($r.Exited) { 'FAIL' } else { 'HANG' }) $r"
            if (-not $ok) { break }  # the state of a failed run is what the watcher is looking at; the next payload starts clean
        }
    }
}

Send-Line 'DEVVM-WEDGE-LOCAL begin'
Measure-Phase 'pwsh-direct' $pwsh
Wait-Agent $false
Measure-Phase 'broker-medium' $shim

& C:\ProgramData\devvm\killagent.ps1 | ForEach-Object { Send-Line "DEVVM-WEDGE-LOCAL killagent: $_" }
Start-ScheduledTask -TaskName 'devvm-agent-elevated'
Wait-Agent $true
Measure-Phase 'broker-elevated' $shim
Send-Line 'DEVVM-WEDGE-LOCAL-DONE'
