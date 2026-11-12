@echo off
rem Spike only (never merged). Run from the answer disk by the windowsPE RunSynchronous (WinPE has no PowerShell).
rem Measured: WinPE has no serial port at this point, and diskpart sees only the answer disk (the storage driver is not
rem loaded yet). So this script:
rem   1. Logs what WinPE looks like to winpe.log on the answer disk (attached writable for this).
rem   2. When partition.flag exists: loads viostor from the virtio CD (drvload), rescans until a disk of 32 GB or more
rem      shows up, and partitions that disk whatever number Windows gave it, so the answer file need not name a disk
rem      (it installs to the first available partition). Without the flag it only logs (the `auto` rule).
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
if not exist "%~dp0partition.flag" exit /b 0
if "%VIRTIO%"=="" goto novirtio
echo -- drvload>> "%LOG%"
drvload %VIRTIO%\viostor\w11\amd64\viostor.inf>> "%LOG%" 2>&1
set TRIES=0
:scan
set /a TRIES+=1
set DISK=
echo rescan> X:\devvm-list.txt
echo list disk>> X:\devvm-list.txt
diskpart /s X:\devvm-list.txt> X:\devvm-list.out 2>&1
for /f "tokens=2,4,5" %%a in ('findstr /b /c:"  Disk " X:\devvm-list.out') do if /i "%%c"=="GB" if %%b GEQ 32 set DISK=%%a
if not "%DISK%"=="" goto found
set /a REM10=TRIES %% 10
if "%REM10%"=="0" echo scan %TRIES%: no disk of 32 GB or more yet>> "%LOG%"
goto scan
:found
echo scans needed: %TRIES%>> "%LOG%"
echo -- disks after>> "%LOG%"
type X:\devvm-list.out>> "%LOG%"
echo disk chosen: [%DISK%]>> "%LOG%"
echo select disk %DISK%> X:\devvm-part.txt
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
