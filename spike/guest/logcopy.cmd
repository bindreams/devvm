@echo off
rem Spike only (never merged). Started detached by winpe.cmd: copies Setup's logs to <answer disk>\logs every pass, so
rem they survive a Setup that stops at an error dialog (the runner only ever reads the answer disk afterwards). Runs until
rem the VM is stopped. cmd built-ins, xcopy, copy and ping only (WinPE).
rem Usage: logcopy.cmd <answer disk folder, trailing backslash>
rem Dry run on a developer PC (touches no real drive): set DEVVM_LOGCOPY_DRIVES=Q and DEVVM_LOGCOPY_ONCE=1.
set "DEST=%~1logs"
if not defined DEVVM_LOGCOPY_DRIVES set "DEVVM_LOGCOPY_DRIVES=X C D E F G H I J K L M N O P Q R S T U V W Y Z"
mkdir "%DEST%" 2>nul
:pass
for %%d in (%DEVVM_LOGCOPY_DRIVES%) do call :drive %%d
if defined DEVVM_LOGCOPY_ONCE exit /b 0
ping -n 31 127.0.0.1 >nul
goto pass

:drive
call :tree "%1:\Windows\Panther" "%1-windows-panther"
call :tree "%1:\$WINDOWS.~BT\Sources\Panther" "%1-bt-panther"
call :tree "%1:\$WINDOWS.~BT\Sources\Rollback" "%1-bt-rollback"
call :file "%1:\Windows\INF\setupapi.dev.log" "%1-setupapi.dev.log"
call :file "%1:\Windows\INF\setupapi.setup.log" "%1-setupapi.setup.log"
exit /b 0

:tree
if not exist "%~1\" exit /b 0
xcopy /y /s /e /i /q /c "%~1" "%DEST%\%~2" >nul 2>&1
exit /b 0

:file
if not exist "%~1" exit /b 0
copy /y "%~1" "%DEST%\%~2" >nul 2>&1
exit /b 0
