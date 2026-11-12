@echo off
rem Spike only (never merged). Run from the answer disk by the windowsPE RunSynchronous (WinPE has no PowerShell).
rem   0. Is found by the answer file at C: too: with a blank system disk the answer disk can be WinPE's C:.
rem   1. Logs what the serial port looks like in WinPE to winpe.log on the answer disk (attached writable for this).
rem   2. When partition.flag exists: partitions the one fixed disk of 32 GB or more, whatever number Windows gave it, so
rem      the answer file need not name a disk (it installs to the first available partition).
set LOG=%~dp0winpe.log
echo winpe.cmd begins> "%LOG%"
echo -- mode>> "%LOG%"
mode>> "%LOG%" 2>&1
echo -- serial ports in the registry>> "%LOG%"
reg query HKLM\HARDWARE\DEVICEMAP\SERIALCOMM>> "%LOG%" 2>&1
echo -- write to COM1>> "%LOG%"
echo DEVVM-PROGRESS=windows-pe started>\\.\COM1 2>> "%LOG%"
echo errorlevel %errorlevel%>> "%LOG%"
echo list disk> X:\devvm-list.txt
diskpart /s X:\devvm-list.txt> X:\devvm-list.out 2>&1
type X:\devvm-list.out>> "%LOG%"
if not exist "%~dp0partition.flag" exit /b 0
set DISK=
for /f "tokens=2,4,5" %%a in ('findstr /b /c:"  Disk " X:\devvm-list.out') do if /i "%%c"=="GB" if %%b GEQ 32 set DISK=%%a
echo disk chosen: [%DISK%]>> "%LOG%"
if "%DISK%"=="" exit /b 1
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
echo DEVVM-PROGRESS=windows-pe disk %DISK% partitioned>\\.\COM1 2>> "%LOG%"
echo winpe.cmd done>> "%LOG%"
exit /b 0
