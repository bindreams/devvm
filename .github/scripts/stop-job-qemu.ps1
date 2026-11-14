# Stops the QEMU machines that a test step of this job left running. They run from the sidecar in the tests' cache below
# D:\devvm-tmp, which nothing else on the runner uses; the path, not the name, picks them.
$ErrorActionPreference = 'Stop'
$cache = 'D:\devvm-tmp\'
Get-CimInstance Win32_Process -Filter "Name LIKE 'qemu-system-%'" |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($cache, [StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object {
        # The listing holds no handle, so its id may be another process's by now. Reading Handle opens one that the object
        # keeps for its later calls, and the id is not reused while it is open: the path is checked, and the process
        # ended and waited for, through that handle.
        $process = Get-Process -Id $_.ProcessId -ErrorAction Ignore
        if ($process) {
            try { $null = $process.Handle } catch { return }
        }
        if (-not $process -or -not $process.Path -or -not $process.Path.StartsWith($cache, [StringComparison]::OrdinalIgnoreCase)) {
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
