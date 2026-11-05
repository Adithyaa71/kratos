@echo off
REM Starts the broker in the foreground, with its log on this console.
REM
REM You do not normally need this. The first tool call an agent makes spawns a
REM broker in about 13ms and it exits again once idle -- that is the design.
REM This is for the times you want to watch it: a new backend, a gate rule you
REM are tuning, or a session where you would rather it did not disappear
REM between tasks.
REM
REM Optional first argument: the pipe name, so one machine can run a broker per
REM agent. It must match KRATOS_PIPE in that agent's MCP config.
REM
REM   start-daemon.cmd                  the default pipe, kratos.v1
REM   start-daemon.cmd kratos.agent-a   a broker of its own
REM
REM Ctrl+C stops it. Closing this window stops it too.
setlocal
set "EXE=%~dp0target\release\kratos-broker.exe"
if not exist "%EXE%" set "EXE=%~dp0target\debug\kratos-broker.exe"
if not exist "%EXE%" (
    echo kratos-broker.exe not found. Build it first:
    echo     cargo build --release
    exit /b 1
)
if not "%~1"=="" set "KRATOS_PIPE=%~1"
if "%KRATOS_PIPE%"=="" set "KRATOS_PIPE=kratos.v1"

REM Stay up while it is being watched. Without this it would exit after the
REM configured idle period and the window would close on you mid-task.
set "KRATOS_IDLE_EXIT_MS=0"

echo Broker on pipe: %KRATOS_PIPE%
echo Ctrl+C to stop.
echo.
"%EXE%" %2 %3 %4 %5
