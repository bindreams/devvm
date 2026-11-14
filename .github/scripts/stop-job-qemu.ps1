# Stops the QEMU machines that a test step of this job left running. They run from the sidecar in the tests' cache below
# D:\devvm-tmp, which nothing else on the runner uses; the path, not the name, picks them.
Get-CimInstance Win32_Process -Filter "Name LIKE 'qemu-system-%'" |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith('D:\devvm-tmp\', [StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object {
        "stopping $($_.ProcessId): $($_.ExecutablePath)"
        # A machine that ended since the listing is what the script is for.
        Stop-Process -Id $_.ProcessId -Force -ErrorAction Ignore
    }
