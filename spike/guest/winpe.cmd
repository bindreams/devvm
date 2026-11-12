@echo off
rem Spike only (never merged). Run from the answer disk by the windowsPE RunSynchronous (WinPE has no PowerShell).
rem Measured: WinPE has no serial port at this point, and diskpart sees only the answer disk until the storage driver is
rem loaded. So this script:
rem   1. Logs what WinPE looks like to winpe.log on the answer disk (attached writable for this).
rem   2. When partition.flag exists: loads viostor from the virtio CD (drvload), rescans until a disk other than the
rem      answer disk shows up, and partitions that disk whatever number Windows gave it, so the answer file need not
rem      name a disk (it installs to the first available partition). Without the flag it only logs (the `auto` rule,
rem      measured: Setup then stops at its disk page).
rem The disk is picked without reading localized text and with cmd built-ins only (no findstr, no `for /f` over a
rem command: neither matched anything in WinPE in three runs, in English and in German). In `list disk` output the
rem rows after the dashed line are `<word> <number> ...`; `detail disk` on each number is searched for the volume label
rem CIDATA (data, not language): that disk is the answer disk, the first other one is the system disk.
rem Replay (on a developer PC, never diskpart): set DEVVM_REPLAY to a folder holding list.txt and detail-<N>.txt, set W to
rem a scratch folder, run `winpe.cmd --replay`; it prints `disk chosen: [N]`.
if not defined W set W=X:
if "%~1"=="--replay" goto replay
set LOG=%~dp0winpe.log
echo winpe.cmd begins> "%LOG%"
echo -- tools and serial ports>> "%LOG%"
ver>> "%LOG%"
if exist "%SystemRoot%\System32\findstr.exe" (echo findstr.exe is present>> "%LOG%") else (echo findstr.exe is absent>> "%LOG%")
if exist "%SystemRoot%\System32\find.exe" (echo find.exe is present>> "%LOG%") else (echo find.exe is absent>> "%LOG%")
reg query HKLM\HARDWARE\DEVICEMAP\SERIALCOMM>> "%LOG%" 2>&1
echo -- volumes>> "%LOG%"
for %%d in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do if exist %%d:\ echo %%d: is present>> "%LOG%"
set VIRTIO=
for %%d in (C D E F G H I J K L M N O P Q R S T U V W Y Z) do if exist %%d:\viostor\w11\amd64\viostor.inf set VIRTIO=%%d:
echo virtio CD: [%VIRTIO%]>> "%LOG%"
echo -- disks before>> "%LOG%"
echo list disk> %W%\devvm-list.txt
diskpart /s %W%\devvm-list.txt> %W%\devvm-list.out 2>&1
type %W%\devvm-list.out>> "%LOG%"
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
pnputil /scan-devices> %W%\devvm-scan.out 2>&1
echo rescan> %W%\devvm-list.txt
echo list disk>> %W%\devvm-list.txt
diskpart /s %W%\devvm-list.txt> %W%\devvm-list.out 2>&1
call :pick
if not "%DISK%"=="" goto found
set /a REM100=TRIES %% 100
if %TRIES% LEQ 3 goto dump
if "%REM100%"=="0" goto dump
goto scan
:dump
echo -- scan %TRIES%: no disk other than the answer disk; diskpart said>> "%LOG%"
type %W%\devvm-list.out>> "%LOG%"
echo -- pnputil /scan-devices said>> "%LOG%"
type %W%\devvm-scan.out>> "%LOG%"
if %TRIES% GTR 2 goto scan
echo -- devices after drvload (pnputil)>> "%LOG%"
pnputil /enum-devices /connected>> "%LOG%" 2>&1
echo -- viostor service>> "%LOG%"
reg query HKLM\SYSTEM\CurrentControlSet\Services\viostor /s>> "%LOG%" 2>&1
goto scan
:found
echo scans needed: %TRIES% >> "%LOG%"
echo -- disks after>> "%LOG%"
type %W%\devvm-list.out>> "%LOG%"
echo disk chosen: [%DISK%]>> "%LOG%"
echo select disk %DISK% > %W%\devvm-part.txt
echo clean>> %W%\devvm-part.txt
echo convert gpt>> %W%\devvm-part.txt
echo create partition efi size=100>> %W%\devvm-part.txt
echo format quick fs=fat32 label=System>> %W%\devvm-part.txt
echo create partition msr size=16>> %W%\devvm-part.txt
echo create partition primary>> %W%\devvm-part.txt
echo format quick fs=ntfs label=Windows>> %W%\devvm-part.txt
echo list partition>> %W%\devvm-part.txt
diskpart /s %W%\devvm-part.txt>> "%LOG%" 2>&1
echo winpe.cmd done>> "%LOG%"
exit /b 0
:novirtio
echo no virtio CD found; no disk chosen>> "%LOG%"
exit /b 1

:replay
set LOG=%W%\winpe-replay.log
set TRIES=1
copy /y "%DEVVM_REPLAY%\list.txt" "%W%\devvm-list.out" >nul
set DISK=
call :pick
echo disk chosen: [%DISK%]
exit /b 0

:pick
rem Reads %W%\devvm-list.out; sets DISK to the first disk that does not hold the CIDATA volume.
set SEEN=0
for /f "usebackq tokens=1,2" %%a in ("%W%\devvm-list.out") do call :row "%%a" "%%b"
exit /b 0

:row
rem %1 %2 are the first two words of a line. A row is a line after the dashed one whose second word is all digits.
set "A=%~1"
set "B=%~2"
if "%A:~0,2%"=="--" set SEEN=1
if not "%SEEN%"=="1" exit /b 0
if "%B%"=="" exit /b 0
for /f "delims=0123456789" %%x in ("%B%") do exit /b 0
call :check %B%
exit /b 0

:check
rem %1 is a disk number. The disk with the CIDATA volume is the answer disk; DISK gets the first other.
if defined DEVVM_REPLAY goto replaydetail
echo select disk %1 > %W%\devvm-d.txt
echo detail disk>> %W%\devvm-d.txt
diskpart /s %W%\devvm-d.txt> %W%\devvm-d.out 2>&1
goto detailed
:replaydetail
copy /y "%DEVVM_REPLAY%\detail-%1.txt" "%W%\devvm-d.out" >nul
:detailed
if %TRIES% LEQ 3 echo -- detail of disk %1 >> "%LOG%"
if %TRIES% LEQ 3 type %W%\devvm-d.out>> "%LOG%"
set ANSWER=0
for /f "usebackq delims=" %%l in ("%W%\devvm-d.out") do call :cidata "%%l"
if "%ANSWER%"=="1" exit /b 0
if "%DISK%"=="" set DISK=%1
exit /b 0

:cidata
set "L=%~1"
set "R=%L:CIDATA=%"
if not "%R%"=="%L%" set ANSWER=1
exit /b 0
