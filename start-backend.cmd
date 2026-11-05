@echo off
REM Starts the perception backend: the thing that reads the screen.
REM It waits for the broker and reconnects on its own, so order does not matter.
setlocal
set "PYEXE=%~dp0.venv\Scripts\python.exe"
if not exist "%PYEXE%" set "PYEXE=python"
echo Starting the Kratos perception backend (Ctrl+C to stop)...
"%PYEXE%" "%~dp0backends\kratos_backend.py" %*
