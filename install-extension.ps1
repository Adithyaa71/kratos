<#
.SYNOPSIS
    Registers the kratos native-messaging host with your browsers.

.DESCRIPTION
    The fiddly part of installing the extension is a JSON manifest that has to
    contain this machine's absolute path to kratos-host.exe and your
    extension's generated id, plus a registry key per browser pointing at it.
    This writes all of that.

    Load the extension first (it has no id until you do):
      1. open brave://extensions  (or chrome://extensions, edge://extensions)
      2. turn on Developer mode
      3. Load unpacked -> pick the `extension` folder next to this script
      4. copy the Extension ID it shows

.PARAMETER ExtensionId
    The id from the extensions page, e.g. aaaabbbbccccddddeeeeffffgggghhhh

.PARAMETER Browsers
    Which browsers to register with. Default: all three.

.EXAMPLE
    .\install-extension.ps1 -ExtensionId aaaabbbbccccddddeeeeffffgggghhhh

.EXAMPLE
    .\install-extension.ps1 -ExtensionId aaaa... -Browsers brave
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[a-p]{32}$')]
    [string]$ExtensionId,

    [ValidateSet('brave', 'chrome', 'edge')]
    [string[]]$Browsers = @('brave', 'chrome', 'edge')
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

# Prefer the release build; fall back to debug so this works mid-development.
$candidates = @(
    (Join-Path $here 'target\release\kratos-host.exe'),
    (Join-Path $here 'target\debug\kratos-host.exe')
)
$hostExe = $candidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $hostExe) {
    throw "kratos-host.exe not found. Run: cargo build --release"
}
$hostExe = (Resolve-Path $hostExe).Path

$stateDir = Join-Path $env:LOCALAPPDATA 'kratos'
if (-not (Test-Path $stateDir)) {
    New-Item -ItemType Directory -Path $stateDir -Force | Out-Null
}
$manifestPath = Join-Path $stateDir 'native-host.json'

# The host name must match HOST in extension/background.js.
$manifest = [ordered]@{
    name            = 'ai.kratos.bridge'
    description     = 'kratos browser bridge'
    path            = $hostExe
    type            = 'stdio'
    allowed_origins = @("chrome-extension://$ExtensionId/")
}

$json = $manifest | ConvertTo-Json -Depth 4
# ASCII and no BOM: Chromium's parser is strict about this file.
[System.IO.File]::WriteAllText($manifestPath, $json, [System.Text.UTF8Encoding]::new($false))

Write-Host "manifest written" -ForegroundColor Green
Write-Host "  $manifestPath"
Write-Host "  host  : $hostExe"
Write-Host "  origin: chrome-extension://$ExtensionId/"
Write-Host ""

# Each Chromium brand reads its own registry hive. Same manifest for all.
$hives = @{
    brave  = 'HKCU:\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\ai.kratos.bridge'
    chrome = 'HKCU:\Software\Google\Chrome\NativeMessagingHosts\ai.kratos.bridge'
    edge   = 'HKCU:\Software\Microsoft\Edge\NativeMessagingHosts\ai.kratos.bridge'
}

foreach ($browser in $Browsers) {
    $key = $hives[$browser]
    try {
        if (-not (Test-Path $key)) {
            New-Item -Path $key -Force | Out-Null
        }
        # The default (unnamed) value holds the manifest path.
        Set-ItemProperty -Path $key -Name '(default)' -Value $manifestPath
        Write-Host ("registered: {0,-7} {1}" -f $browser, $key) -ForegroundColor Green
    }
    catch {
        Write-Host ("failed:     {0,-7} {1}" -f $browser, $_.Exception.Message) -ForegroundColor Red
    }
}

Write-Host ""
Write-Host "Next:" -ForegroundColor Cyan
Write-Host "  1. Fully quit and reopen the browser. The native host is only"
Write-Host "     looked up at extension startup, so a reload is not enough."
Write-Host "  2. Reload any tabs you want readable. Content scripts are"
Write-Host "     injected at page load, so tabs open before the extension was"
Write-Host "     installed report 'not readable here'."
Write-Host "  3. Click the extension icon. Broker should read 'connected' once"
Write-Host "     a broker is running."
Write-Host ""
Write-Host "To undo:" -ForegroundColor DarkGray
foreach ($browser in $Browsers) {
    Write-Host "  Remove-Item '$($hives[$browser])' -Force" -ForegroundColor DarkGray
}
