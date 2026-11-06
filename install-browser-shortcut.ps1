<#
.SYNOPSIS
    Makes the kratos bridge load every time you open your browser normally.

.DESCRIPTION
    The extension has to be loaded from disk with --load-extension. Installing
    it through brave://extensions instead writes a record into the profile's
    Secure Preferences, and that record can end up damaged: the browser runs
    the extension but refuses to resolve a native messaging host for it, and
    the only symptom is "Specified native messaging host not found" against a
    registration that is provably correct.

    A launcher script fixes that only for people who remember to use the
    launcher. This puts the flag on the shortcuts you actually click -- Start
    Menu, taskbar, desktop -- so opening the browser the normal way loads the
    bridge.

    Every change is idempotent and reversible: run with -Undo to strip the
    flag back out.

.PARAMETER Browser
    brave (default), chrome or edge.

.PARAMETER Undo
    Remove the flag from every shortcut it was added to.

.EXAMPLE
    .\install-browser-shortcut.ps1

.EXAMPLE
    .\install-browser-shortcut.ps1 -Undo
#>

[CmdletBinding()]
param(
    [ValidateSet('brave', 'chrome', 'edge')]
    [string]$Browser = 'brave',
    [switch]$Undo
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$extension = Join-Path $here 'extension'

if (-not (Test-Path (Join-Path $extension 'manifest.json'))) {
    throw "No extension folder next to this script: $extension"
}

$exeName = @{ brave = 'brave.exe'; chrome = 'chrome.exe'; edge = 'msedge.exe' }[$Browser]
$flag = "--load-extension=`"$extension`""

# Everywhere Windows keeps a shortcut you might click.
$roots = @(
    [Environment]::GetFolderPath('Desktop'),
    [Environment]::GetFolderPath('CommonDesktopDirectory'),
    (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
    (Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs'),
    (Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar'),
    (Join-Path $env:APPDATA 'Microsoft\Internet Explorer\Quick Launch')
) | Where-Object { $_ -and (Test-Path $_) } | Select-Object -Unique

$shell = New-Object -ComObject WScript.Shell
$touched = 0
$skipped = 0

foreach ($root in $roots) {
    Get-ChildItem -Path $root -Filter *.lnk -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
        $lnk = $_.FullName
        try { $sc = $shell.CreateShortcut($lnk) } catch { return }
        if (-not $sc.TargetPath) { return }
        if ((Split-Path $sc.TargetPath -Leaf) -ine $exeName) { return }

        $args = $sc.Arguments
        $has = $args -match [regex]::Escape('--load-extension=')

        if ($Undo) {
            if (-not $has) { return }
            # Strip any --load-extension argument, quoted or bare.
            $new = ($args -replace '--load-extension="[^"]*"', '' -replace '--load-extension=\S+', '').Trim()
        }
        else {
            if ($args -like "*$flag*") {
                Write-Host ("already set:  {0}" -f $lnk) -ForegroundColor DarkGray
                $skipped++
                return
            }
            # Replace a stale one rather than stacking a second.
            $base = ($args -replace '--load-extension="[^"]*"', '' -replace '--load-extension=\S+', '').Trim()
            $new = if ($base) { "$base $flag" } else { $flag }
        }

        $sc.Arguments = $new
        $sc.Save()
        Write-Host ("{0}: {1}" -f $(if ($Undo) { 'cleared' } else { 'patched' }), $lnk) -ForegroundColor Green
        $touched++
    }
}

Write-Host ''
if ($touched -eq 0 -and $skipped -eq 0) {
    Write-Host "No $exeName shortcuts found. Launch it with start-my-brave.cmd instead." -ForegroundColor Yellow
    exit 0
}
Write-Host ("{0} shortcut(s) {1}, {2} already correct." -f $touched, $(if ($Undo) { 'cleared' } else { 'patched' }), $skipped)

if (-not $Undo) {
    Write-Host ''
    Write-Host 'Now close the browser completely -- and on Windows that is not what' -ForegroundColor Cyan
    Write-Host 'closing the windows does. Background processes keep it alive and the' -ForegroundColor Cyan
    Write-Host 'next click just joins them, flag and all ignored:' -ForegroundColor Cyan
    Write-Host ''
    Write-Host "    Get-Process brave,chrome,msedge -ErrorAction SilentlyContinue | Stop-Process -Force" -ForegroundColor DarkGray
    Write-Host ''
    Write-Host 'Then open it from the shortcut as usual. Also remove the extension from' -ForegroundColor Cyan
    Write-Host 'the extensions page if you installed it there: two entries claiming the' -ForegroundColor Cyan
    Write-Host 'same id is what caused this.' -ForegroundColor Cyan
}
