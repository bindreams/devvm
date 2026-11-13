# Spike only (never merged). Run by the host through the Medium agent (session 1, the user's own token), PowerShell 5.1:
# is Windows Terminal this image's default terminal, and what the agent's console is hosted by. `key=value` lines.
$ErrorActionPreference = 'Continue'

function Show([string]$Name, [scriptblock]$Block) {
    try { $text = (& $Block 2>&1 | Out-String).Trim() -replace '\s*\r?\n\s*', ' | ' } catch { $text = "ERROR $($_.Exception.Message)" }
    "$Name=$text"
}

Show 'os' { $v = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'; "$($v.ProductName) $($v.DisplayVersion) build $($v.CurrentBuild).$($v.UBR) edition $($v.EditionID)" }
# DelegationTerminal: all zeros (or unset) is "let Windows decide" (Windows Terminal when it is installed),
# {B23D10C0-E52E-411E-9D5B-C09FDF709C7D} Windows Console Host, {E12CFF52-A866-4C77-9A90-F570A7AA2C6B} Windows Terminal.
Show 'hkcu-console-startup' { Get-ItemProperty -LiteralPath 'HKCU:\Console\%%Startup' | Select-Object DelegationConsole, DelegationTerminal | Format-List }
Show 'hkcu-console' { Get-ItemProperty -LiteralPath 'HKCU:\Console' | Select-Object * -ExcludeProperty PS* | Format-List }
Show 'hklm-console-startup' { Get-ItemProperty -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Console\%%Startup' | Select-Object DelegationConsole, DelegationTerminal | Format-List }
Show 'appx-user' { Get-AppxPackage -Name 'Microsoft.WindowsTerminal*' | Select-Object Name, Version, InstallLocation | Format-List }
Show 'appx-provisioned' { Get-AppxProvisionedPackage -Online | Where-Object { $_.DisplayName -match 'Terminal' } | Select-Object DisplayName, Version | Format-List }
Show 'wt-command' { (Get-Command wt.exe -ErrorAction Stop).Source }
Show 'terminal-processes' {
    Get-CimInstance Win32_Process -Filter "Name='WindowsTerminal.exe' OR Name='OpenConsole.exe' OR Name='conhost.exe' OR Name='ssh-broker.exe'" |
        Sort-Object ProcessId | ForEach-Object {
            $parent = Get-CimInstance Win32_Process -Filter "ProcessId=$($_.ParentProcessId)"
            "$($_.Name)#$($_.ProcessId) session=$($_.SessionId) parent=$($parent.Name)#$($_.ParentProcessId) created=$($_.CreationDate.ToUniversalTime().ToString('o')) cmd=$($_.CommandLine)"
        }
}
Show 'terminal-windows' {
    Get-Process -Name WindowsTerminal, conhost, OpenConsole, ssh-broker -ErrorAction SilentlyContinue |
        ForEach-Object { "$($_.ProcessName)#$($_.Id) handle=$($_.MainWindowHandle) title=$($_.MainWindowTitle)" }
}
Show 'session' { "$([Diagnostics.Process]::GetCurrentProcess().SessionId) as $([Security.Principal.WindowsIdentity]::GetCurrent().Name)" }
