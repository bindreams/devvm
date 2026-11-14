# Stops the QEMU machines that a test step of this job left running. They run from the sidecar in the tests' cache below
# D:\devvm-tmp, which nothing else on the runner uses; the path, not the name, picks them.
$ErrorActionPreference = 'Stop'
$cache = 'D:\devvm-tmp\'
Get-CimInstance Win32_Process -Filter "Name LIKE 'qemu-system-%'" |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($cache, [StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object {
        # The listing holds no handle, so its id may be another process's by now. Reading Handle first opens one that the
        # object keeps, and Windows does not reuse an id while a handle to it is open: the path read by id after it, the
        # kill and the wait all reach the process the handle holds.
        $process = Get-Process -Id $_.ProcessId -ErrorAction Ignore
        if (-not $process) { return }
        try {
            $null = $process.Handle
        } catch {
            # A machine that ended since the listing is what the script is for; any other failure fails the step. An ended
            # process cannot be opened to ask, so its id is looked up again.
            if (Get-Process -Id $process.Id -ErrorAction Ignore) { throw }
            return
        }
        if (-not $process.Path -or -not $process.Path.StartsWith($cache, [StringComparison]::OrdinalIgnoreCase)) {
            return
        }
        "stopping $($process.Id): $($process.Path)"
        try {
            $process.Kill()
        } catch {
            # A machine that ended since the listing is what the script is for; any other failure fails the step.
            if (-not $process.HasExited) { throw }
        }
        # The next step starts once the machine is gone, not once it was told to go.
        $process.WaitForExit()
    }
