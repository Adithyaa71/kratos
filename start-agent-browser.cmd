@echo off
REM Launch a browser bound to one agent's broker and one Chrome/Brave profile.
REM
REM   start-agent-browser.cmd agent-a "Profile 1"
REM   start-agent-browser.cmd agent-b "Profile 2"
REM
REM Why this works: the browser inherits KRATOS_PIPE from this shell, and the
REM native-messaging host it launches inherits it in turn. So that browser's
REM extension talks to that agent's broker and no other. Point the agent's MCP
REM server at the same KRATOS_PIPE and the two are paired.
REM
REM Browser actions go through DOM events rather than the mouse, so two agents
REM running this way genuinely work in parallel. Native (non-browser) actions
REM still share one physical pointer -- see the skill file.

setlocal

if "%~1"=="" (
  echo usage: start-agent-browser.cmd ^<agent-name^> ["Profile Directory"] [browser]
  echo.
  echo   agent-name  becomes the pipe suffix, e.g. agent-a  -^> kratos.agent-a
  echo   profile     Chrome/Brave profile directory name, default "Default"
  echo   browser     brave ^| chrome ^| edge, default brave
  exit /b 1
)

set "AGENT=%~1"
set "PROFILE=%~2"
if "%PROFILE%"=="" set "PROFILE=Default"
set "BROWSER=%~3"
if "%BROWSER%"=="" set "BROWSER=brave"

REM The pipe this browser's extension will reach.
set "KRATOS_PIPE=kratos.%AGENT%"

if /i "%BROWSER%"=="brave" (
  set "EXE=%LOCALAPPDATA%\BraveSoftware\Brave-Browser\Application\brave.exe"
  if not exist "!EXE!" set "EXE=%ProgramFiles%\BraveSoftware\Brave-Browser\Application\brave.exe"
) else if /i "%BROWSER%"=="chrome" (
  set "EXE=%ProgramFiles%\Google\Chrome\Application\chrome.exe"
) else if /i "%BROWSER%"=="edge" (
  set "EXE=%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe"
) else (
  echo unknown browser: %BROWSER%
  exit /b 1
)

setlocal enabledelayedexpansion
if not exist "!EXE!" (
  echo Could not find %BROWSER% at:
  echo   !EXE!
  echo Edit this script with the right path.
  exit /b 1
)

echo Launching %BROWSER%
echo   profile : %PROFILE%
echo   pipe    : %KRATOS_PIPE%
echo.
echo Point this agent's MCP server at the same pipe:
echo   "env": { "KRATOS_PIPE": "%KRATOS_PIPE%", "KRATOS_AGENT_NAME": "%AGENT%" }
echo.

start "" "!EXE!" --profile-directory="%PROFILE%"
endlocal
