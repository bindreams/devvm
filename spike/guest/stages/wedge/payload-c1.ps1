# generated from spike/m5_carriers.py: c1(["emit", "cat"]). The wedge stage runs it with 1 MiB on stdin.
$ErrorActionPreference = 'Stop'
if ([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) { [Console]::Error.WriteLine('devvm: the command did not reach the user''s session'); exit 126 }
$env:PATH = 'C:\ProgramData\devvm\m5\bin' + ';' + $env:PATH
$app = Get-Command -CommandType Application -Name 'emit' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $app) { [Console]::Error.WriteLine('devvm: ' + 'emit' + ': command not found'); exit 127 }
$PSNativeCommandArgumentPassing = 'Standard'
& $app.Source 'cat'
exit $LASTEXITCODE
