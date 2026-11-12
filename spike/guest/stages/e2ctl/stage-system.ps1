# Spike only (never merged). Stage `e2ctl`, boot task (SYSTEM): the E2 control of the wedge root-cause note (0.6), with no
# sshd and no network. pipectl.exe gives a child the three standard streams as OpenSSH's fileio_pipe lays them out
# (overlapped child ends, `ovl`) or as CreatePipe does (synchronous child ends, `sync`), sends 1 MiB to its stdin, and
# closes the stdin end right after (`early`) or after the whole echo came back (`late`, E2). Readers: `emit cat`, a
# plain ReadFile reader, and powershell.exe 5.1 with [Console]::In.ReadToEnd(). Each cell is one pipectl.exe run of up to
# 20 samples; a hang costs its own 60 s bound and ends only its cell.
param([Parameter(Mandatory)][string]$Root)
. C:\ProgramData\devvm\lib.ps1
. (Join-Path $Root 'ssh-common.ps1')

Copy-Item -LiteralPath (Join-Path $Root 'lib.ps1'), (Join-Path $Root 'hook.ps1') -Destination C:\ProgramData\devvm -Force
. C:\ProgramData\devvm\lib.ps1  # the copy sends the time with every line

$ErrorActionPreference = 'Continue'  # pipectl.exe's stderr is data here, not a terminating error
$bin = 'C:\ProgramData\devvm\m5\bin'
$dir = 'C:\ProgramData\devvm\e2ctl'
Step 'HELPERS' {
    New-Item -ItemType Directory -Force -Path $bin, $dir | Out-Null
    Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'm5helper.cs')) -OutputType ConsoleApplication -OutputAssembly "$bin\emit.exe"
    Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'readfile.cs')) -OutputType ConsoleApplication -OutputAssembly "$dir\readfile.exe"
    Add-Type -TypeDefinition (Get-Content -Raw -LiteralPath (Join-Path $Root 'pipectl.cs')) -OutputType ConsoleApplication -OutputAssembly "$dir\pipectl.exe"
    Get-ChildItem $bin, $dir | Select-Object Name, Length | Out-String
}

$cells = @(
    @('emit', 'ovl', 'late'), @('emit', 'sync', 'late'), @('emit', 'ovl', 'early'), @('emit', 'sync', 'early'),
    @('readfile', 'ovl', 'late'), @('readfile', 'sync', 'late'), @('readfile', 'ovl', 'early'), @('readfile', 'sync', 'early'),
    @('ps5', 'ovl', 'early'), @('ps5', 'sync', 'early')
)
foreach ($cell in $cells) {
    Send-Line "DEVVM-E2CTL-CELL=$($cell -join ' ') begins"
    & "$dir\pipectl.exe" $cell[0] $cell[1] $cell[2] 20 60 2>&1 | ForEach-Object { Send-Line "DEVVM-E2CTL $_" }
    Send-Line "DEVVM-E2CTL-CELL=$($cell -join ' ') ended, pipectl exit $LASTEXITCODE"
}
Send-Line 'DEVVM-E2CTL-DONE'
