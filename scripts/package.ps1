# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
    Builds DriveMount's release artefacts (spec §9, plan M2-10) into target\package.

.DESCRIPTION
    - DriveMount-<version>-x64.msi: the MSI, from apps\drivemount\wix\main.wxs by cargo-wix;
    - DriveMount-<version>-setup.exe: a WiX Burn bundle that installs WinFsp first if it is missing or
      older than 2.1, then the MSI (apps\drivemount\setup);
    - DriveMount-<version>-portable-x64.zip: drivemount.exe on its own.
    Each has LICENSE.txt and THIRD-PARTY-LICENSES.html (cargo-about, NFR-LIC-04).

    Needs: cargo-wix and cargo-about (`cargo install`), the WiX Toolset 3.14 binaries (-Wix, or the WIX
    environment variable), and the unmodified WinFsp MSI from winfsp.dev (-WinFspMsi).
    Signing is M2-11's: the exe and MSI before the bundle is made, the bundle's engine with insignia.

.PARAMETER Version
    The version the installers carry. The workspace's by default; a test of an upgrade gives another.

.PARAMETER TargetDir
    Cargo's target folder for the release build. Another than `target` leaves a running
    target\release\drivemount.exe alone.

.PARAMETER OutDir
    Where the artefacts go; target\package by default. It is emptied first.

.PARAMETER NoBuild
    Use the release exe already built.
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Wix = $(if ($env:WIX) { $env:WIX } else { 'C:\marcin\tools\wix314' }),
    [string]$WinFspMsi = 'C:\marcin\tools\winfsp\winfsp-2.2.26286.msi',
    [string]$TargetDir,
    [string]$OutDir,
    [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$app = Join-Path $root 'apps\drivemount'
if (-not $TargetDir) { $TargetDir = Join-Path $root 'target' }
$release = Join-Path $TargetDir 'release'
$out = if ($OutDir) { $OutDir } else { Join-Path $root 'target\package' }
$stage = Join-Path $out 'stage'
$icon = Join-Path $app 'ui\tray\idle.ico'

function Invoke-Checked {
    param([string]$What, [scriptblock]$Command)
    Write-Host "== $What"
    # Windows PowerShell takes a native program's stderr (cargo's progress) for an error under Stop.
    $ErrorActionPreference = 'Continue'
    & $Command
    $ErrorActionPreference = 'Stop'
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit code $LASTEXITCODE)" }
}

# The WiX binaries folder: candle.exe is in it, or in its bin (an installed toolset).
$wixBin = @($Wix, (Join-Path $Wix 'bin')) | Where-Object { Test-Path (Join-Path $_ 'candle.exe') } |
    Select-Object -First 1
if (-not $wixBin) { throw "WiX 3.14 not found at $Wix (give -Wix or set WIX)" }
if (-not (Test-Path $WinFspMsi)) { throw "WinFsp MSI not found at $WinFspMsi (give -WinFspMsi)" }

if (-not $Version) {
    $metadata = cargo metadata --format-version 1 --no-deps --manifest-path (Join-Path $root 'Cargo.toml') |
        ConvertFrom-Json
    $Version = ($metadata.packages | Where-Object { $_.name -eq 'drivemount' }).version
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "an MSI version is three numbers, not '$Version'" }

if (-not $NoBuild) {
    Invoke-Checked 'release build' { cargo build --release -p drivemount --target-dir $TargetDir --manifest-path (Join-Path $root 'Cargo.toml') }
}
$exe = Join-Path $release 'drivemount.exe'
if (-not (Test-Path $exe)) { throw "$exe is not there; build without -NoBuild" }

if (Test-Path $out) { Remove-Item -Recurse -Force $out }
New-Item -ItemType Directory -Force $stage | Out-Null

Invoke-Checked 'third-party licences' {
    cargo about generate --manifest-path (Join-Path $app 'Cargo.toml') -c (Join-Path $root 'about.toml') `
        (Join-Path $root 'packaging\about.hbs') -o (Join-Path $stage 'THIRD-PARTY-LICENSES.html')
}
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')
# Beside the release exe too, so the About page's button finds it in a local run.
Copy-Item (Join-Path $stage 'THIRD-PARTY-LICENSES.html') $release

$msi = Join-Path $out "DriveMount-$Version-x64.msi"
Invoke-Checked 'MSI' {
    cargo wix --package drivemount --no-build --nocapture `
        --target-bin-dir $release --bin-path $wixBin --install-version $Version --output $msi `
        --compiler-arg "-dStage=$stage" --compiler-arg "-dIcon=$icon" `
        --linker-arg '-sice:ICE57'
    # ICE57 takes the Start menu for a per-user folder, as it does not look at ALLUSERS; in this
    # per-machine package it is the all-users one, and the shortcut's key path is under HKLM.
}

$setup = Join-Path $out "DriveMount-$Version-setup.exe"
$obj = Join-Path $out 'obj'
New-Item -ItemType Directory -Force $obj | Out-Null
$setupDir = Join-Path $app 'setup'
# light copies the theme files in as they are, and a fault in them shows only when setup starts.
foreach ($file in 'theme.xml', 'theme.wxl') {
    $document = New-Object -ComObject Msxml2.DOMDocument.6.0
    $document.async = $false
    if (-not $document.load((Join-Path $setupDir $file))) {
        $fault = $document.parseError
        throw "$file, line $($fault.line): $($fault.reason)"
    }
}
Invoke-Checked 'setup exe: candle' {
    & (Join-Path $wixBin 'candle.exe') -nologo -arch x64 -ext WixBalExtension -ext WixUtilExtension `
        "-dVersion=$Version" "-dMsi=$msi" "-dWinFspMsi=$WinFspMsi" "-dIcon=$icon" "-dSetupDir=$setupDir" `
        -out (Join-Path $obj 'bundle.wixobj') (Join-Path $setupDir 'bundle.wxs')
}
Invoke-Checked 'setup exe: light' {
    & (Join-Path $wixBin 'light.exe') -nologo -ext WixBalExtension -ext WixUtilExtension `
        -out $setup (Join-Path $obj 'bundle.wixobj')
}

$zip = Join-Path $out "DriveMount-$Version-portable-x64.zip"
$portable = Join-Path $out 'portable'
New-Item -ItemType Directory -Force $portable | Out-Null
Copy-Item $exe, (Join-Path $stage 'LICENSE.txt'), (Join-Path $stage 'THIRD-PARTY-LICENSES.html') $portable
Compress-Archive -Path (Join-Path $portable '*') -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -Recurse -Force $portable, $obj
Remove-Item (Join-Path $out '*.wixpdb')

Write-Host '== done'
Get-ChildItem $out -File | Format-Table Name, Length
