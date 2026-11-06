<#
.SYNOPSIS
    Makes the browser load the kratos bridge every time, with no flags and
    nothing to remember.

.DESCRIPTION
    Loading an extension unpacked has two failure modes that look identical
    from the outside and are invisible from inside the browser:

      * installed through the extensions page, the profile keeps a record of
        it in Secure Preferences -- and that record can end up damaged. The
        browser still runs the extension, and still refuses to resolve a
        native messaging host for it. Forever, against a registration that is
        provably correct, with nothing in any log.

      * loaded with --load-extension, it works, but only for a browser that
        was started with the flag. Click the taskbar icon instead and the
        flag is gone. On Windows, so is any hope of noticing: closing the
        windows leaves background processes alive, so the next launch joins
        them and the flag is ignored even when you did use the shortcut.

    So this stops loading it unpacked. It packs the extension into a signed
    .crx, writes a local update manifest, and force-installs it through
    enterprise policy -- the mechanism a company uses to put an extension on
    ten thousand machines and keep it there.

    A force-installed extension is always enabled, cannot be turned off or
    removed from the extensions page, has its permissions granted up front,
    and loads at every start of every window. There is no condition left to
    get wrong.

    The signing key is kept, so the extension id is stable across re-runs.
    Re-run this after changing anything in extension/ and the browser picks
    up the new version by itself.

.PARAMETER Browser
    brave (default), chrome or edge.

.PARAMETER Undo
    Remove the policy and the native-host registration.
#>

[CmdletBinding()]
param(
    [ValidateSet('brave', 'chrome', 'edge')]
    [string]$Browser = 'brave',
    [switch]$Undo
)

$ErrorActionPreference = 'Stop'
$here = Split-Path -Parent $MyInvocation.MyCommand.Path

$BRANDS = @{
    brave  = @{ exe = @("$env:LOCALAPPDATA\BraveSoftware\Brave-Browser\Application\brave.exe",
                        "$env:ProgramFiles\BraveSoftware\Brave-Browser\Application\brave.exe")
                policy = 'Software\Policies\BraveSoftware\Brave-Browser'
                hives = @('Software\BraveSoftware\Brave-Browser',
                          'Software\BraveSoftware\Brave-Browser-Beta',
                          'Software\BraveSoftware\Brave-Browser-Nightly',
                          'Software\Chromium') }
    chrome = @{ exe = @("$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
                        "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe")
                policy = 'Software\Policies\Google\Chrome'
                hives = @('Software\Google\Chrome', 'Software\Chromium') }
    edge   = @{ exe = @("${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe",
                        "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe")
                policy = 'Software\Policies\Microsoft\Edge'
                hives = @('Software\Microsoft\Edge') }
}

$brand = $BRANDS[$Browser]
$state = Join-Path $env:LOCALAPPDATA 'kratos\browser'
$crx = Join-Path $state 'kratos.crx'
$pem = Join-Path $state 'kratos.pem'
$xml = Join-Path $state 'update.xml'
$policyKey = "HKCU:\$($brand.policy)\ExtensionInstallForcelist"
$hostName = 'ai.kratos.bridge'

function Url([string]$path) { 'file:///' + ($path -replace '\\', '/') }

# ---------------------------------------------------------------- undo
if ($Undo) {
    if (Test-Path $policyKey) {
        Get-ItemProperty $policyKey | Get-Member -MemberType NoteProperty |
            Where-Object { (Get-ItemProperty $policyKey).($_.Name) -like '*kratos*' -or $true } |
            Out-Null
        Remove-Item $policyKey -Recurse -Force
        Write-Host "removed policy: $policyKey" -ForegroundColor Green
    }
    foreach ($h in $brand.hives) {
        $k = "HKCU:\$h\NativeMessagingHosts\$hostName"
        if (Test-Path $k) { Remove-Item $k -Force; Write-Host "removed: $k" -ForegroundColor Green }
    }
    Write-Host ''
    Write-Host 'Quit the browser completely for this to take effect:' -ForegroundColor Cyan
    Write-Host '    Get-Process brave,chrome,msedge -ErrorAction SilentlyContinue | Stop-Process -Force' -ForegroundColor DarkGray
    exit 0
}

# ---------------------------------------------------------------- checks
$exe = $brand.exe | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $exe) { throw "Could not find $Browser. Looked in:`n  " + ($brand.exe -join "`n  ") }

$source = Join-Path $here 'extension'
if (-not (Test-Path (Join-Path $source 'manifest.json'))) { throw "No extension folder beside this script: $source" }

$hostExe = @((Join-Path $here 'target\release\kratos-host.exe'),
             (Join-Path $here 'target\debug\kratos-host.exe')) |
           Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $hostExe) { throw 'kratos-host.exe not found. Run: cargo build --release' }

New-Item -ItemType Directory -Path $state -Force | Out-Null

# ---------------------------------------------------------------- pack
# A fresh copy, because --pack-extension writes its output beside the source
# and refuses a folder that already contains a build.
$work = Join-Path $state 'src'
if (Test-Path $work) { Remove-Item $work -Recurse -Force }
Copy-Item -Recurse $source $work

$packArgs = @("--pack-extension=$work")
if (Test-Path $pem) { $packArgs += "--pack-extension-key=$pem" }   # keep the id stable
Write-Host "packing $source" -ForegroundColor Cyan
$p = Start-Process -FilePath $exe -ArgumentList $packArgs -PassThru -Wait -WindowStyle Hidden
Start-Sleep -Seconds 2

$outCrx = Join-Path $state 'src.crx'
$outPem = Join-Path $state 'src.pem'
if (-not (Test-Path $outCrx)) { throw "$Browser did not produce a .crx. Is it already running? Quit it and retry." }
Move-Item $outCrx $crx -Force
if (Test-Path $outPem) { Move-Item $outPem $pem -Force }
Remove-Item $work -Recurse -Force
Write-Host "packed: $crx" -ForegroundColor Green

# ---------------------------------------------------------------- id
# The id of a packed extension is the first 16 bytes of the SHA-256 of its
# public key, each nibble written as a..p. Those 16 bytes are already in the
# CRX3 header as crx_id, so read them from there and depend on nothing.
$bytes = [System.IO.File]::ReadAllBytes($crx)
if ([System.Text.Encoding]::ASCII.GetString($bytes[0..3]) -ne 'Cr24') { throw 'not a CRX file' }
# The key sits in the CRX3 header as a DER SubjectPublicKeyInfo, which is
# self-describing -- 30 82 <len-hi> <len-lo> then that many bytes -- so the
# protobuf around it does not have to be parsed at all.
$pk = $null
for ($i = 12; $i -lt 400; $i++) {
    if ($bytes[$i] -eq 0x30 -and $bytes[$i+1] -eq 0x82 -and
        $bytes[$i+4] -eq 0x30 -and $bytes[$i+5] -eq 0x0D) {
        $len = ([int]$bytes[$i+2] -shl 8) + [int]$bytes[$i+3] + 4
        $pk = New-Object byte[] $len
        [Array]::Copy($bytes, $i, $pk, 0, $len)
        break
    }
}
if (-not $pk) { throw 'could not find the public key in the CRX header' }
$digest = [System.Security.Cryptography.SHA256]::Create().ComputeHash([byte[]]$pk)
$extId = -join ($digest[0..15] | ForEach-Object {
    [char](97 + ($_ -shr 4)) + [char](97 + ($_ -band 0x0F))
})
Write-Host "extension id: $extId" -ForegroundColor Green

# ---------------------------------------------------------------- version
$version = (Get-Content (Join-Path $source 'manifest.json') -Raw | ConvertFrom-Json).version

# ---------------------------------------------------------------- update manifest
@"
<?xml version='1.0' encoding='UTF-8'?>
<gupdate xmlns='http://www.google.com/update2/response' protocol='2.0'>
  <app appid='$extId'>
    <updatecheck codebase='$(Url $crx)' version='$version' />
  </app>
</gupdate>
"@ | Set-Content -Path $xml -Encoding UTF8
Write-Host "update manifest: $xml" -ForegroundColor Green

# ---------------------------------------------------------------- native host
# Both ids are allowed: the packed one this installs, and the path-derived one
# an unpacked copy would get, so a developer loading extension/ by hand still
# works without re-registering anything.
$unpackedId = 'ldgfddpecnjhdahallkmdlpiglnflige'
$manifestDir = Join-Path $env:LOCALAPPDATA 'kratos'
$manifestPath = Join-Path $manifestDir 'native-host.json'
New-Item -ItemType Directory -Path $manifestDir -Force | Out-Null
$origins = @("chrome-extension://$extId/")
if ($unpackedId -ne $extId) { $origins += "chrome-extension://$unpackedId/" }
$manifest = [ordered]@{
    name            = $hostName
    description     = 'kratos browser bridge'
    path            = (Resolve-Path $hostExe).Path
    type            = 'stdio'
    allowed_origins = $origins
}
[System.IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 4),
    [System.Text.UTF8Encoding]::new($false))
Write-Host "native host manifest: $manifestPath" -ForegroundColor Green

foreach ($h in $brand.hives) {
    $k = "HKCU:\$h\NativeMessagingHosts\$hostName"
    New-Item -Path $k -Force | Out-Null
    New-ItemProperty -Path $k -Name '(default)' -Value $manifestPath -PropertyType String -Force | Out-Null
    $back = (Get-ItemProperty $k).'(default)'
    if ($back -ne $manifestPath) { Write-Host "WRONG: $k holds $back" -ForegroundColor Red }
    else { Write-Host "registered: $k" -ForegroundColor Green }
}

# ---------------------------------------------------------------- policy
# ExtensionInstallForcelist is how an organisation puts an extension on a
# machine and keeps it there. Force-installed means: always enabled, cannot be
# removed from the extensions page, permissions granted up front, loaded at
# every start. No flag, no launcher, no condition.
New-Item -Path $policyKey -Force | Out-Null
$entry = "$extId;$(Url $xml)"
$slot = $null
for ($n = 1; $n -le 50; $n++) {
    $existing = (Get-ItemProperty -Path $policyKey -ErrorAction SilentlyContinue)."$n"
    if (-not $existing -or $existing -like "$extId;*") { $slot = $n; break }
}
if (-not $slot) { throw 'no free slot in ExtensionInstallForcelist' }
New-ItemProperty -Path $policyKey -Name "$slot" -Value $entry -PropertyType String -Force | Out-Null
Write-Host "policy: $policyKey\$slot = $entry" -ForegroundColor Green

Write-Host ''
Write-Host 'Done. One thing left, and it is the one that always bites:' -ForegroundColor Cyan
Write-Host ''
Write-Host '    Get-Process brave,chrome,msedge -ErrorAction SilentlyContinue | Stop-Process -Force' -ForegroundColor Yellow
Write-Host ''
Write-Host 'Closing the windows is not quitting -- background processes keep the' -ForegroundColor Cyan
Write-Host 'browser alive and the next launch just joins them. After that, open the' -ForegroundColor Cyan
Write-Host 'browser however you like. The bridge will be there, every time.' -ForegroundColor Cyan
Write-Host ''
Write-Host 'If you previously loaded extension/ unpacked, remove it from the' -ForegroundColor DarkGray
Write-Host 'extensions page -- two entries for one bridge is just confusing.' -ForegroundColor DarkGray
