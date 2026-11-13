@echo off
rem Run from the answer disk by the windowsPE RunSynchronous (WinPE has no PowerShell). Written with LF here; devvm
rem writes it to the answer disk with CRLF, which cmd needs for its labels.
rem
rem Measured: WinPE has no serial port at this point, and diskpart sees only the answer disk until the storage driver is
rem loaded. So this script logs to logs\ on the answer disk (attached writable for this), loads viostor from the virtio-win
rem CD (drvload), rescans until a disk other than the answer disk shows up, and partitions that disk whatever number
rem Windows gave it. The answer file names no disk and Setup installs to the partition made here.
rem
rem Nothing is found by drive letter (WinPE can give the answer disk C:) and no text of diskpart's is read for its
rem meaning (it is localized). In `list disk` output the rows after the dashed line are `<word> <number> ...`;
rem `detail disk` on each number is searched for the volume label CIDATA (data, not language): that disk is the answer
rem disk, the first other one is the system disk. Only cmd built-ins are used for parsing: findstr is absent from WinPE.
if not defined W set W=X:
if "%~1"=="--replay" goto replay
if "%~1"=="--logcopy" goto logcopy
if not exist "%~dp0logs" mkdir "%~dp0logs"
set LOG=%~dp0logs\winpe.log
echo winpe.cmd begins> "%LOG%"
start "" /b cmd /c call "%~f0" --logcopy "%~dp0" >nul 2>&1
ver>> "%LOG%"
set VIRTIO=
for %%d in (@@!DRIVE_LETTERS@@) do if exist %%d:\viostor\w11\amd64\viostor.inf set VIRTIO=%%d:
echo virtio CD: [%VIRTIO%]>> "%LOG%"
if "%VIRTIO%"=="" goto novirtio
echo -- drvload>> "%LOG%"
drvload %VIRTIO%\viostor\w11\amd64\viostor.inf>> "%LOG%" 2>&1
if errorlevel 1 goto drvloadfailed
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
rem The first three misses and every hundredth after are logged; the loop itself ends only when a disk shows.
set /a REM100=TRIES %% 100
if %TRIES% LEQ 3 goto dump
if "%REM100%"=="0" goto dump
goto scan
:dump
echo -- scan %TRIES%: no disk other than the answer disk; diskpart said>> "%LOG%"
type %W%\devvm-list.out>> "%LOG%"
echo -- pnputil /scan-devices said>> "%LOG%"
type %W%\devvm-scan.out>> "%LOG%"
goto scan
:found
echo scans needed: %TRIES% >> "%LOG%"
echo -- disks after>> "%LOG%"
type %W%\devvm-list.out>> "%LOG%"
echo disk chosen: [%DISK%]>> "%LOG%"
rem EFI, MSR and one Windows partition to the end of the disk. No recovery partition: it would sit behind C: and
rem stop the boot task from extending C: into a larger disk.
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
if errorlevel 1 goto partitionfailed
echo winpe.cmd done>> "%LOG%"
exit /b 0
:novirtio
echo no virtio CD found; no disk chosen>> "%LOG%"
echo @@!WINPE_FAILED@@=no virtio CD>> "%LOG%"
exit /b 1
:drvloadfailed
echo @@!WINPE_FAILED@@=drvload>> "%LOG%"
exit /b 1
:partitionfailed
echo @@!WINPE_FAILED@@=diskpart>> "%LOG%"
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

:logcopy
rem Started detached by the main part: copies Setup's logs to <answer disk>\logs on every pass, so they survive a Setup
rem that stops at an error dialog (the host reads only the answer disk afterwards). Each is copied as <letter>-<name>:
rem WinPE's setupapi.dev.log and the installed image's are two files. Runs until the VM stops. Named files only, a
rem failed copy is ignored: a full disk loses a log, never the build.
set "DEST=%~2logs"
:pass
for %%d in (X @@!DRIVE_LETTERS@@) do call :drive %%d
ping -n 31 127.0.0.1 >nul
goto pass

:drive
call :copylog "%1:\$WINDOWS.~BT\Sources\Panther\setupact.log" %1-setupact.log
call :copylog "%1:\$WINDOWS.~BT\Sources\Panther\setuperr.log" %1-setuperr.log
call :copylog "%1:\Windows\INF\setupapi.dev.log" %1-setupapi.dev.log
exit /b 0

:copylog
if not exist "%~1" exit /b 0
copy /y "%~1" "%DEST%\%2" >nul 2>&1
exit /b 0
