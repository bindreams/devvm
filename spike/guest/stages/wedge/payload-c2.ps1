# generated from spike/m5_carriers.py: c2(["emit", "cat"]). The wedge stage runs it with 1 MiB on stdin.
$ErrorActionPreference = 'Stop'
if ([Diagnostics.Process]::GetCurrentProcess().SessionId -eq 0) { [Console]::Error.WriteLine('devvm: the command did not reach the user''s session'); exit 126 }
$env:PATH = 'C:\ProgramData\devvm\m5\bin' + ';' + $env:PATH
$app = Get-Command -CommandType Application -Name 'emit' -ErrorAction SilentlyContinue | Select-Object -First 1
if (-not $app) { [Console]::Error.WriteLine('devvm: ' + 'emit' + ': command not found'); exit 127 }
$psi = [Diagnostics.ProcessStartInfo]::new($app.Source)
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.ArgumentList.Add('cat')
$p = [Diagnostics.Process]::Start($psi)
$stdout = [Console]::OpenStandardOutput()
$stderr = [Console]::OpenStandardError()
$ti = [Console]::OpenStandardInput().CopyToAsync($p.StandardInput.BaseStream)
$to = $p.StandardOutput.BaseStream.CopyToAsync($stdout)
$te = $p.StandardError.BaseStream.CopyToAsync($stderr)
$both = [Threading.Tasks.Task]::WhenAll($to, $te)
if ([Threading.Tasks.Task]::WaitAny([Threading.Tasks.Task[]]@($ti, $both)) -eq 0) { try { $p.StandardInput.Close() } catch { } }
$both.Wait()
$p.WaitForExit()
exit $p.ExitCode
