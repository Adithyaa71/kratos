<#
.SYNOPSIS
    Harden one Brave profile for agent use, leaving your other profiles alone.

.DESCRIPTION
    Edits that profile's Preferences file directly, because Brave's settings
    are per profile and its Windows policies are not -- a policy would apply
    to your personal browsing too, which is the opposite of what this is for.

    Turns off: password manager and autofill, payment and address autofill,
    Brave Wallet, Brave Rewards, sync.
    Turns on:  ask-where-to-save for downloads, block camera / microphone /
               location / notifications / popups.
    Leaves alone: cookies and logins (the agent is meant to arrive signed in),
               and Brave Shields (turning those down to make pages easier to
               script is exactly the wrong trade).

    Brave must be fully closed. A backup is written beside the file.

.PARAMETER ProfileDirectory
    The folder name from brave://version -> Profile Path, e.g. "Profile 1".

.PARAMETER Revert
    Restore the most recent backup instead of applying changes.

.EXAMPLE
    .\harden-agent-profile.ps1 -ProfileDirectory "Profile 1"

.EXAMPLE
    .\harden-agent-profile.ps1 -ProfileDirectory "Profile 1" -Revert
#>

[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ProfileDirectory,

    [string]$UserDataRoot = "$env:LOCALAPPDATA\BraveSoftware\Brave-Browser\User Data",

    [switch]$Revert
)

$ErrorActionPreference = 'Stop'

$prefsPath = Join-Path $UserDataRoot (Join-Path $ProfileDirectory 'Preferences')
if (-not (Test-Path $prefsPath)) {
    throw "No Preferences at $prefsPath`nCheck brave://version -> Profile Path for the right folder name."
}

if (Get-Process -Name brave -ErrorAction SilentlyContinue) {
    throw "Brave is running. Close it completely (check the tray) and run this again -- Brave rewrites Preferences on exit and would undo these changes."
}

$backup = "$prefsPath.kratos-backup"

if ($Revert) {
    if (-not (Test-Path $backup)) { throw "No backup at $backup" }
    Copy-Item $backup $prefsPath -Force
    Write-Host "restored $prefsPath from backup" -ForegroundColor Green
    return
}

Copy-Item $prefsPath $backup -Force
Write-Host "backup written: $backup" -ForegroundColor DarkGray

$prefs = Get-Content $prefsPath -Raw -Encoding UTF8 | ConvertFrom-Json

# Walk/create a dotted path and set a leaf value.
function Set-Pref($root, [string]$path, $value) {
    $parts = $path.Split('.')
    $node = $root
    for ($i = 0; $i -lt $parts.Length - 1; $i++) {
        $key = $parts[$i]
        if (-not $node.PSObject.Properties[$key]) {
            $node | Add-Member -NotePropertyName $key -NotePropertyValue ([pscustomobject]@{}) -Force
        }
        $node = $node.$key
    }
    $leaf = $parts[-1]
    $node | Add-Member -NotePropertyName $leaf -NotePropertyValue $value -Force
}

# 2 = block, in Chromium's content-settings vocabulary.
$BLOCK = 2

$changes = [ordered]@{
    # credentials and money stay out of the agent's reach
    'credentials_enable_service'                            = $false
    'credentials_enable_autosignin'                         = $false
    'autofill.profile_enabled'                              = $false
    'autofill.credit_card_enabled'                          = $false
    'payments.can_make_payment_enabled'                     = $false
    'brave.wallet.default_wallet'                           = 0
    'brave.rewards.enabled'                                 = $false
    # keep the agent profile from bleeding into the personal one
    'sync.requested'                                        = $false
    # a page should not be able to drop a file silently
    'download.prompt_for_download'                          = $true
    # hardware and attention, denied by default
    'profile.default_content_setting_values.media_stream_camera' = $BLOCK
    'profile.default_content_setting_values.media_stream_mic'    = $BLOCK
    'profile.default_content_setting_values.geolocation'         = $BLOCK
    'profile.default_content_setting_values.notifications'       = $BLOCK
    'profile.default_content_setting_values.popups'              = $BLOCK
}

foreach ($entry in $changes.GetEnumerator()) {
    Set-Pref $prefs $entry.Key $entry.Value
    Write-Host ("  {0,-56} -> {1}" -f $entry.Key, $entry.Value)
}

# Depth matters: Preferences nests deeply and ConvertTo-Json truncates at 2.
$prefs | ConvertTo-Json -Depth 100 -Compress |
    Set-Content $prefsPath -Encoding UTF8 -NoNewline

Write-Host ""
Write-Host "hardened: $ProfileDirectory" -ForegroundColor Green
Write-Host "cookies and existing logins were left untouched, and so were Brave Shields."
Write-Host ""
Write-Host "To undo:  .\harden-agent-profile.ps1 -ProfileDirectory '$ProfileDirectory' -Revert" -ForegroundColor DarkGray
