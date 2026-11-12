# Spike only (never merged). Stage `uac`, logon task (the user, Medium). Waits for the SYSTEM part.
#   default:     M6 E-B: can the Medium session start the elevated helper task (default descriptor, user-run descriptor),
#                what does the helper's process see, how long does it take; then E0(b): `Start-Process -Verb RunAs` with
#                the default UAC: does it prompt (consent.exe) or run.
#   autoapprove: M7: the same RunAs call with ConsentPromptBehaviorAdmin=0.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1

$variant = (Get-Content -LiteralPath (Join-Path $Root 'variant.txt') -ErrorAction SilentlyContinue | Select-Object -First 1)
while (-not (Test-Path -LiteralPath C:\Users\Public\devvm-ready-system)) { Start-Sleep -Milliseconds 500 }
Send-Line "DEVVM-UAC-VARIANT=$variant who=$(whoami) session $(Get-SessionId) integrity $(Get-IntegritySid)"

function Send-File([string]$Name, [string]$Path) {
    Send-Line "DEVVM-$Name=$((Get-Content -LiteralPath $Path -Encoding UTF8 -ErrorAction SilentlyContinue) -join ' | ')"
}

# The Medium session's own facts, for the comparison.
& (Join-Path $Root 'helper-probe.ps1') -Out C:\Users\Public\devvm-medium-out.txt
Send-File 'MEDIUM-FACTS' C:\Users\Public\devvm-medium-out.txt

function Start-Helper([string]$Name) {
    # Three ways to start a task from Medium; the first that works is used.
    $before = (Get-ScheduledTaskInfo -TaskName $Name -ErrorAction SilentlyContinue).LastRunTime
    $started = $false
    foreach ($way in 'COM', 'Start-ScheduledTask', 'schtasks') {
        try {
            switch ($way) {
                'COM' {
                    $service = New-Object -ComObject Schedule.Service
                    $service.Connect()
                    [void]$service.GetFolder('\').GetTask($Name).Run([Type]::Missing)
                }
                'Start-ScheduledTask' { Start-ScheduledTask -TaskName $Name -ErrorAction Stop | Out-Null }
                'schtasks' {
                    $text = schtasks.exe /run /tn $Name 2>&1 | Out-String
                    if ($LASTEXITCODE -ne 0) { throw $text }
                }
            }
            Send-Line "DEVVM-HELPER-START-$Name=$way worked"
            $started = $true
            break
        } catch {
            Send-Line "DEVVM-HELPER-START-$Name=$way failed: $($_.Exception.Message)"
        }
    }
    return @{ Started = $started; Before = $before }
}

function Wait-Helper([string]$Name, [string]$Out, $Before) {
    # Ends with the helper's done file, or with the task having run and ended without writing one.
    while (-not (Test-Path -LiteralPath "$Out.done")) {
        $info = Get-ScheduledTaskInfo -TaskName $Name -ErrorAction SilentlyContinue
        if ($info -and $info.LastRunTime -ne $Before -and (Get-ScheduledTask -TaskName $Name).State -eq 'Ready') {
            return (Test-Path -LiteralPath "$Out.done")
        }
        Start-Sleep -Milliseconds 200
    }
    return $true
}

if ($variant -eq 'default') {
    foreach ($name in 'devvm-helper-default', 'devvm-helper-sddl') {
        $out = "C:\Users\Public\$name-out.txt"
        foreach ($run in 'cold', 'warm') {
            Remove-Item -Path "$out*" -ErrorAction SilentlyContinue
            $clock = [Diagnostics.Stopwatch]::StartNew()
            $start = Start-Helper $name
            if (-not $start.Started) { break }
            $finished = Wait-Helper $name $out $start.Before
            $clock.Stop()
            Send-Line "DEVVM-HELPER-$name-$run=finished=$finished milliseconds=$($clock.ElapsedMilliseconds) result=$((Get-ScheduledTaskInfo -TaskName $name).LastTaskResult)"
            if ($finished) { Send-File "HELPER-$name-FACTS" $out }
        }
    }
}

# M7 (autoapprove) and E0(b) (default): a RunAs launch from the Medium session.
$out = 'C:\Users\Public\devvm-runas-out.txt'
Remove-Item -Path "$out*" -ErrorAction SilentlyContinue
# Start-Process returns as soon as the child is launched, so the job ending says nothing about the child: the job hands
# back the child's process id, and the wait ends with the child's file, a consent prompt, a failed launch or the child gone.
$job = Start-Job -ScriptBlock {
    (Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -PassThru -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'C:\ProgramData\devvm\helper-probe.ps1', '-Out', 'C:\Users\Public\devvm-runas-out.txt')).Id
}
$clock = [Diagnostics.Stopwatch]::StartNew()
$childId = $null
while (-not (Test-Path -LiteralPath "$out.done") -and -not (Get-Process consent -ErrorAction SilentlyContinue)) {
    if ($job.State -ne 'Running') {
        if ($job.State -ne 'Completed') { break }
        if ($null -eq $childId) { $childId = @(Receive-Job $job -Keep)[0] }
        if ($null -eq $childId -or -not (Get-Process -Id $childId -ErrorAction SilentlyContinue)) { break }
    }
    Start-Sleep -Milliseconds 200
}
$clock.Stop()
Send-Line "DEVVM-RUNAS-$variant=child-ran=$(Test-Path -LiteralPath "$out.done") consent-prompt=$([bool](Get-Process consent -ErrorAction SilentlyContinue)) job=$($job.State) child-id=$childId milliseconds=$($clock.ElapsedMilliseconds)"
Send-Line "DEVVM-RUNAS-JOB=$(Receive-Job $job -Keep -ErrorAction SilentlyContinue 2>&1 | Out-String) reason=$($job.ChildJobs[0].JobStateInfo.Reason)"
if (Test-Path -LiteralPath "$out.done") { Send-File "RUNAS-$variant-FACTS" $out }
