# Starts the pinned QEMU as the devvm tests' bare machine does and lists the processes it started (CI only).
param([string]$Qemu, [string]$Dir)
New-Item -ItemType Directory -Force $Dir | Out-Null
$sock = Join-Path $Dir 'q.sock'
$arguments = @(
    '-machine', 'none', '-accel', 'tcg', '-display', 'none', '-nodefaults', '-no-user-config', '-S',
    '-chardev', "socket,id=qmp,path=$sock,server=on,wait=off",
    '-object', 'monitor-qmp,id=qmp0,chardev=qmp',
    '-netdev', 'user,id=n0,hostfwd=tcp:127.0.0.1:0-:22'
)
$process = Start-Process -FilePath $Qemu -ArgumentList $arguments -PassThru -WindowStyle Hidden
try {
    # Waits for the socket file, which exists once QEMU is initialised.
    while (-not (Test-Path $sock)) {
        if ($process.HasExited) { throw "QEMU exited ($($process.ExitCode)) before binding its socket" }
    }
    $all = Get-CimInstance Win32_Process
    $found = @()
    $queue = @($process.Id)
    while ($queue.Count -gt 0) {
        $next = @()
        foreach ($parent in $queue) {
            foreach ($child in ($all | Where-Object { $_.ParentProcessId -eq $parent })) {
                $found += "$($child.ProcessId) $($child.Name)"
                $next += $child.ProcessId
            }
        }
        $queue = $next
    }
    Write-Output "RESULT qemu-children qemu_pid=$($process.Id) descendants=[$($found -join '; ')] count=$($found.Count)"
} finally {
    Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
}
