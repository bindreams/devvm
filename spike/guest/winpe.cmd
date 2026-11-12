@echo off
rem Spike only (never merged). Run from the answer disk by the windowsPE RunSynchronous (WinPE has no PowerShell).
rem Measured: WinPE has no serial port at this point, and diskpart sees only the answer disk (the storage driver is not
rem loaded yet). So this script:
rem   1. Logs what WinPE looks like to winpe.log on the answer disk (attached writable for this).
rem   2. When partition.flag exists: loads viostor from the virtio CD (drvload), rescans until a disk other than the
rem      answer disk shows up, and partitions that disk whatever number Windows gave it, so the answer file need not
rem      name a disk (it installs to the first available partition). Without the flag it only logs (the `auto` rule,
rem      measured: Setup then stops at its disk page).
rem The disk is picked without reading localized text: diskpart prints "Disk" as "Datentraeger" on a German WinPE
rem and the size unit differs by language too. Every row of `list disk` (two spaces, a word, a number) names a disk
rem number; `detail disk` on each lists its volumes, and the disk that holds the CIDATA volume (data, not language)
rem is the answer disk. The system disk is the first other one.
rem The log holds diskpart's and pnputil's own text, not only verdicts: after drvload at scans 1, 2, 3 and every 100th.
set LOG=%~dp0winpe.log
echo winpe.cmd begins> "%LOG%"
echo -- serial ports in the registry>> "%LOG%"
reg query HKLM\HARDWARE\DEVICEMAP\SERIALCOMM>> "%LOG%" 2>&1
echo -- volumes>> "%LOG%"
for %%d in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do if exist %%d:\ echo %%d: is present>> "%LOG%"
set VIRTIO=
for %%d in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do if exist %%d:\viostor\w11\amd64\viostor.inf set VIRTIO=%%d:
echo virtio CD: [%VIRTIO%]>> "%LOG%"
echo -- disks before>> "%LOG%"
echo list disk> X:\devvm-list.txt
diskpart /s X:\devvm-list.txt> X:\devvm-list.out 2>&1
type X:\devvm-list.out>> "%LOG%"
echo -- devices before (pnputil)>> "%LOG%"
pnputil /enum-devices /connected>> "%LOG%" 2>&1
if not exist "%~dp0partition.flag" exit /b 0
if "%VIRTIO%"=="" goto novirtio
echo -- drvload>> "%LOG%"
drvload %VIRTIO%\viostor\w11\amd64\viostor.inf>> "%LOG%" 2>&1
set TRIES=0
:scan
set /a TRIES+=1
set DISK=
pnputil /scan-devices> X:\devvm-scan.out 2>&1
echo rescan> X:\devvm-list.txt
echo list disk>> X:\devvm-list.txt
diskpart /s X:\devvm-list.txt> X:\devvm-list.out 2>&1
for /f "tokens=2" %%n in ('findstr /b /r /c:"  [^ ][^ ]*  *[0-9]" X:\devvm-list.out') do call :check %%n
if not "%DISK%"=="" goto found
set /a REM100=TRIES %% 100
if %TRIES% LEQ 3 goto dump
if "%REM100%"=="0" goto dump
goto scan
:dump
echo -- scan %TRIES%: no disk other than the answer disk; diskpart said>> "%LOG%"
type X:\devvm-list.out>> "%LOG%"
echo -- pnputil /scan-devices said>> "%LOG%"
type X:\devvm-scan.out>> "%LOG%"
if %TRIES% GTR 2 goto scan
echo -- devices after drvload (pnputil)>> "%LOG%"
pnputil /enum-devices /connected>> "%LOG%" 2>&1
echo -- viostor service>> "%LOG%"
reg query HKLM\SYSTEM\CurrentControlSet\Services\viostor /s>> "%LOG%" 2>&1
goto scan
:found
echo scans needed: %TRIES% >> "%LOG%"
echo -- disks after>> "%LOG%"
type X:\devvm-list.out>> "%LOG%"
echo disk chosen: [%DISK%]>> "%LOG%"
echo select disk %DISK% > X:\devvm-part.txt
echo clean>> X:\devvm-part.txt
echo convert gpt>> X:\devvm-part.txt
echo create partition efi size=100>> X:\devvm-part.txt
echo format quick fs=fat32 label=System>> X:\devvm-part.txt
echo create partition msr size=16>> X:\devvm-part.txt
echo create partition primary>> X:\devvm-part.txt
echo format quick fs=ntfs label=Windows>> X:\devvm-part.txt
echo list partition>> X:\devvm-part.txt
diskpart /s X:\devvm-part.txt>> "%LOG%" 2>&1
echo winpe.cmd done>> "%LOG%"
exit /b 0
:novirtio
echo no virtio CD found; no disk chosen>> "%LOG%"
exit /b 1
:check
rem %1 is a disk number from `list disk`. The disk with the CIDATA volume is the answer disk; DISK gets the first other.
echo select disk %1 > X:\devvm-d.txt
echo detail disk>> X:\devvm-d.txt
diskpart /s X:\devvm-d.txt> X:\devvm-d.out 2>&1
if %TRIES% LEQ 3 echo -- detail of disk %1 >> "%LOG%"
if %TRIES% LEQ 3 type X:\devvm-d.out>> "%LOG%"
findstr /c:"CIDATA" X:\devvm-d.out >nul
if not errorlevel 1 exit /b 0
if "%DISK%"=="" set DISK=%1
exit /b 0
