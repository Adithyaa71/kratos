@echo off
REM Launch a browser that belongs to one agent: its own profile, its own
REM broker, and the extension loaded fresh from this repository.
REM
REM   start-agent-browser.cmd agent-a
REM   start-agent-browser.cmd agent-b chrome
REM
REM Three things this gets right that sharing your everyday browser does not:
REM
REM   1. Its own --user-data-dir, so this really is a new browser process. A
REM      --profile-directory alone joins the one already running, which then
REM      inherits none of the environment below and none of the isolation.
REM   2. KRATOS_PIPE is inherited by the browser and by the native host it
REM      launches, so this browser's extension reaches this agent's broker and
REM      no other.
REM   3. --load-extension reads the extension off disk at every launch. No
REM      install state to go stale, no service worker left over from a browser
REM      that did not really exit, and the id stays the same because it is
REM      derived from the path.
REM
REM Browser actions go through DOM events rather than the mouse, so two agents
REM running this way genuinely work in parallel. Native (non-browser) actions
REM still share one physical pointer -- see the skill file.

setlocal enabledelayedexpansion

if "%~1"=="" (
  echo usage: start-agent-browser.cmd ^<agent-name^> [browser]
  echo.
  echo   agent-name  becomes the pipe suffix, e.g. agent-a  -^> kratos.agent-a
  echo   browser     brave ^| chrome ^| edge, default brave
  exit /b 1
)

set "AGENT=%~1"
set "BROWSER=%~2"
if "%BROWSER%"=="" set "BROWSER=brave"

REM The pipe this browser's extension will reach.
set "KRATOS_PIPE=kratos.%AGENT%"

REM Its own profile on disk, kept beside the rest of the state.
set "USERDATA=%LOCALAPPDATA%\kratos\browsers\%AGENT%"
set "EXTENSION=%~dp0extension"

if /i "%BROWSER%"=="brave" (
  set "EXE=%LOCALAPPDATA%\BraveSoftware\Brave-Browser\Application\brave.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles%\BraveSoftware\Brave-Browser\Application\brave.exe"
) else if /i "%BROWSER%"=="chrome" (
  set "EXE=%ProgramFiles%\Google\Chrome\Application\chrome.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles(x86)%\Google\Chrome\Application\chrome.exe"
) else if /i "%BROWSER%"=="edge" (
  set "EXE=%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles%\Microsoft\Edge\Application\msedge.exe"
) else (
  echo unknown browser: %BROWSER%
  exit /b 1
)

if not exist "!EXE!" (
  echo Could not find %BROWSER% at:
  echo   !EXE!
  echo Edit this script with the right path.
  exit /b 1
)

if not exist "!EXTENSION!\manifest.json" (
  echo The extension folder is not next to this script:
  echo   !EXTENSION!
  exit /b 1
)

echo Launching %BROWSER%
echo   profile   : !USERDATA!
echo   extension : !EXTENSION!
echo   pipe      : %KRATOS_PIPE%
echo.
echo Point this agent's MCP server at the same pipe:
echo   "env": { "KRATOS_PIPE": "%KRATOS_PIPE%", "KRATOS_AGENT_NAME": "%AGENT%" }
echo.

start "" "!EXE!" --user-data-dir="!USERDATA!" --load-extension="!EXTENSION!" --no-first-run --no-default-browser-check
endlocal
