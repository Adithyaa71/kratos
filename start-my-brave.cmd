@echo off
REM Start YOUR everyday browser -- your profile, your logins, your tabs -- with
REM the kratos extension loaded from this folder.
REM
REM   start-my-brave.cmd            Brave
REM   start-my-brave.cmd chrome     Chrome
REM   start-my-brave.cmd edge       Edge
REM
REM Why a flag instead of installing it from brave://extensions:
REM
REM An unpacked extension installed through the extensions page is recorded in
REM the profile's Secure Preferences, and that record can end up damaged -- on
REM this machine one did, with an empty `state` field. The browser still runs
REM the extension, so the popup opens and content scripts work, and it still
REM refuses to resolve a native messaging host for it. The symptom is
REM "Specified native messaging host not found" against a registry entry that
REM is provably correct, forever, with nothing to see in any log.
REM
REM --load-extension reads the folder at every launch and keeps no such record.
REM The id is derived from the path, so it does not change and the native-host
REM registration keeps working. Remove the installed copy from the extensions
REM page first, or the browser will have two entries claiming the same id.
REM
REM IMPORTANT: no --user-data-dir here, on purpose. This is your real profile.

setlocal enabledelayedexpansion

set "BROWSER=%~1"
if "%BROWSER%"=="" set "BROWSER=brave"
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
  exit /b 1
)
if not exist "!EXTENSION!\manifest.json" (
  echo The extension folder is not next to this script:
  echo   !EXTENSION!
  exit /b 1
)

REM The flag only takes effect on a browser that is actually starting. Closing
REM the windows is not enough on Windows: background processes keep the browser
REM alive, and the new window just joins them.
tasklist /fi "imagename eq %~n0" >nul 2>&1
for %%B in (brave.exe chrome.exe msedge.exe) do (
  tasklist /fi "imagename eq %%B" 2>nul | find /i "%%B" >nul && (
    echo %%B is already running. It has to exit completely first:
    echo     powershell -Command "Get-Process brave,chrome,msedge -ErrorAction SilentlyContinue ^| Stop-Process -Force"
    echo Closing the windows is not enough -- background processes keep it alive.
    exit /b 1
  )
)

echo Starting %BROWSER% with the kratos bridge
echo   extension : !EXTENSION!
echo   profile   : your normal one -- logins and tabs intact
echo.
start "" "!EXE!" --load-extension="!EXTENSION!"
endlocal
