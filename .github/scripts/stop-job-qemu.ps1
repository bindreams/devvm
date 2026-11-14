# Stops the QEMU machines that a test step of this job left running. They run from the sidecar in the tests' cache below
# D:\devvm-tmp, which nothing else on the runner uses; the path, not the name, picks them.
$ErrorActionPreference = 'Stop'
Get-CimInstance Win32_Process -Filter "Name LIKE 'qemu-system-%'" |
    Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith('D:\devvm-tmp\', [StringComparison]::OrdinalIgnoreCase) } |
    ForEach-Object {
        "stopping $($_.ProcessId): $($_.ExecutablePath)"
        try {
            Stop-Process -Id $_.ProcessId -Force
            # The next step starts once the machine is gone, not once it was told to go.
            Wait-Process -Id $_.ProcessId
        } catch {
            # A machine that ended since the listing is what the script is for; any other failure fails the step.
            if ($_.FullyQualifiedErrorId -notlike 'NoProcessFoundForGivenId*') { throw }
        }
    }
