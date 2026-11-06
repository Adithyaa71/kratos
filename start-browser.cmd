@echo off
REM Start a browser that belongs to one harness, using YOUR profile and logins.
REM
REM   start-browser.cmd claude brave      Brave   -> pipe kratos.v1
REM   start-browser.cmd bluee  vivaldi    Vivaldi -> pipe kratos.bluee
REM
REM One browser per harness. The browser inherits KRATOS_PIPE from this script
REM and the native-messaging host it launches inherits it in turn, so that
REM browser's extension reaches that harness's broker and no other. Point the
REM harness's MCP server at the same pipe and the two are paired.
REM
REM This uses your normal profile on purpose -- no --user-data-dir -- because a
REM browser with none of your logins in it is not useful. One profile per agent
REM is a separate thing and is not built yet.
REM
REM The extension is loaded from disk rather than installed through the
REM extensions page. Installing it there writes a record into the profile's
REM Secure Preferences, and a damaged record produces a browser that runs the
REM extension and still refuses to resolve a native messaging host for it --
REM "Specified native messaging host not found", forever, against a correct
REM registration. Loading from disk keeps no record. (setup-browser.ps1 is the
REM permanent answer: it force-installs by policy and needs no flag at all.)

setlocal enabledelayedexpansion

if "%~1"=="" (
  echo usage: start-browser.cmd ^<harness^> [browser]
  echo.
  echo   harness   claude ^| bluee ^| any name. claude means the default pipe.
  echo   browser   brave ^| vivaldi ^| chrome ^| edge, default brave
  exit /b 1
)

set "HARNESS=%~1"
set "BROWSER=%~2"
if "%BROWSER%"=="" set "BROWSER=brave"
set "EXTENSION=%~dp0extension"

REM `claude` is the default broker, so its pipe is the default name.
if /i "%HARNESS%"=="claude" (set "KRATOS_PIPE=kratos.v1") else (set "KRATOS_PIPE=kratos.%HARNESS%")

if /i "%BROWSER%"=="brave" (
  set "EXE=%LOCALAPPDATA%\BraveSoftware\Brave-Browser\Application\brave.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles%\BraveSoftware\Brave-Browser\Application\brave.exe"
  set "IMAGE=brave.exe"
) else if /i "%BROWSER%"=="vivaldi" (
  set "EXE=%LOCALAPPDATA%\Vivaldi\Application\vivaldi.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles%\Vivaldi\Application\vivaldi.exe"
  set "IMAGE=vivaldi.exe"
) else if /i "%BROWSER%"=="chrome" (
  set "EXE=%ProgramFiles%\Google\Chrome\Application\chrome.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles(x86)%\Google\Chrome\Application\chrome.exe"
  set "IMAGE=chrome.exe"
) else if /i "%BROWSER%"=="edge" (
  set "EXE=%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles%\Microsoft\Edge\Application\msedge.exe"
  set "IMAGE=msedge.exe"
) else (
  echo unknown browser: %BROWSER%
  exit /b 1
)

if not exist "!EXE!" (
  echo Could not find %BROWSER% at:
  echo   !EXE!
  exit /b 1
)
if not exist "!EXTENSION!\manifest.json" (
  echo The extension folder is not next to this script:
  echo   !EXTENSION!
  exit /b 1
)

REM A flag means nothing to a browser that is only being joined, and on Windows
REM closing every window does not end one: background processes keep it alive.
tasklist /fi "imagename eq !IMAGE!" 2>nul | find /i "!IMAGE!" >nul && (
  echo !IMAGE! is already running, so it would be joined rather than started
  echo and nothing below would take effect. End it completely first:
  echo.
  echo     powershell -Command "Get-Process !IMAGE:.exe=! -ErrorAction SilentlyContinue ^| Stop-Process -Force"
  echo.
  exit /b 1
)

echo Launching %BROWSER% for %HARNESS%
echo   pipe      : %KRATOS_PIPE%
echo   extension : !EXTENSION!
echo   profile   : your normal one -- logins and tabs intact
echo.
echo Point %HARNESS%'s MCP server at the same pipe:
echo   "env": { "KRATOS_PIPE": "%KRATOS_PIPE%", "KRATOS_AGENT_NAME": "%HARNESS%" }
echo.

start "" "!EXE!" --load-extension="!EXTENSION!"
endlocal
