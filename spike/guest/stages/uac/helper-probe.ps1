# Spike only (never merged). What a process sees: run by the elevated scheduled-task helper (M6 E-B), by `Start-Process
# -Verb RunAs` (M7) and by the Medium session itself (the comparison). Writes <Out>, then <Out>.done.
param([string]$Out = 'C:\Users\Public\devvm-helper-out.txt')
$lines = New-Object System.Collections.Generic.List[string]
function Add-Fact([string]$Key, $Value) { $lines.Add("$Key=$Value") }
try {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    Add-Fact 'who-sid' $identity.User.Value
    Add-Fact 'session' ([Diagnostics.Process]::GetCurrentProcess().SessionId)
    $principal = New-Object Security.Principal.WindowsPrincipal $identity
    Add-Fact 'elevated-role' $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    $integrity = (whoami /groups /fo csv | ConvertFrom-Csv | Where-Object { $_.SID -like 'S-1-16-*' } | Select-Object -First 1).SID
    Add-Fact 'integrity' $integrity
    Add-Fact 'hkcu-loaded' (Test-Path 'HKCU:\Software')
    foreach ($name in 'USERPROFILE', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'USERNAME', 'Path') {
        Add-Fact "env-$name" ([Environment]::GetEnvironmentVariable($name))
    }
    Add-Type -AssemblyName System.Security
    $secret = [Text.Encoding]::UTF8.GetBytes('devvm')
    $protected = [Security.Cryptography.ProtectedData]::Protect($secret, $null, 'CurrentUser')
    $back = [Security.Cryptography.ProtectedData]::Unprotect($protected, $null, 'CurrentUser')
    Add-Fact 'dpapi' ([Text.Encoding]::UTF8.GetString($back))
    Add-Fact 'winget' (winget --version 2>&1 | Out-String).Trim()
    cmd.exe /c 'exit 3221225477'
    Add-Fact 'exit-code-0xC0000005' $LASTEXITCODE
} catch {
    Add-Fact 'error' ($_ | Out-String)
}
Set-Content -LiteralPath $Out -Value $lines -Encoding UTF8
Set-Content -LiteralPath "$Out.done" -Value (Get-Date -Format o)
