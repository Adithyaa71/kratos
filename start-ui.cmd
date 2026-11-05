@echo off
REM Opens the kratos settings window.
REM
REM This is the one you want if you are not sure which one you want. From it:
REM
REM   Status          start and stop the broker, see what it can see
REM   Browser         paste your extension id, register the native host
REM   Kill switch     halt everything, or one browser, or one app
REM   Decision model  which model, its key, and the idle timeout
REM   Confidence      how certain it has to be, per kind of action
REM   Gate            what is never allowed and what needs approval
REM   Log             what was decided, and what actually changed
REM
REM Double-click this file, or run it from a shell. Closing the window does not
REM stop a broker an agent is using.
setlocal
set "EXE=%~dp0target\release\kratos-ui.exe"
if not exist "%EXE%" set "EXE=%~dp0target\debug\kratos-ui.exe"
if not exist "%EXE%" (
    echo kratos-ui.exe not found. Build it first:
    echo     cargo build --release
    pause
    exit /b 1
)
start "" "%EXE%"
