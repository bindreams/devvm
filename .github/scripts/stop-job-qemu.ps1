# Stops the QEMU machines that a test step of this job left running. They run from the sidecar in the tests' cache below
# D:\devvm-tmp, which nothing else on the runner uses; the path, not the name, picks them.
$ErrorActionPreference = 'Stop'
$cache = 'D:\devvm-tmp\'
Get-CimInstance Win32_Process -Filter "Name LIKE 'qemu-system-%'" |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith($cache, [StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object {
        # The listing holds no handle, so its id may be another process's by now: the path is read again from a handle,
        # and the same handle is ended and waited for.
        $process = Get-Process -Id $_.ProcessId -ErrorAction Ignore
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
