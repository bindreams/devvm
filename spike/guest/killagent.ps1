# Spike only (never merged). Ends every ssh-broker agent process and returns when they are gone (M6 E0 (a), E-C).
$agents = @(Get-CimInstance Win32_Process -Filter "Name='ssh-broker.exe'" | Where-Object { $_.CommandLine -match '\bagent\b' })
foreach ($agent in $agents) {
    "killing $($agent.ProcessId) session $($agent.SessionId): $($agent.CommandLine)"
    Stop-Process -Id $agent.ProcessId -Force -ErrorAction SilentlyContinue
}
foreach ($agent in $agents) { Wait-Process -Id $agent.ProcessId -ErrorAction SilentlyContinue }
"killed $($agents.Count)"
