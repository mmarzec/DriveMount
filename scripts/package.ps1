# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
    Builds DriveMount's release artefacts (spec §9, plan M2-10) into target\package.

.DESCRIPTION
    - DriveMount-<version>-x64.msi: the MSI, from apps\drivemount\wix\main.wxs by cargo-wix;
    - DriveMount-<version>-setup.exe: a WiX Burn bundle that installs WinFsp first if it is missing or
      older than 2.1, then the MSI (apps\drivemount\setup);
    - DriveMount-<version>-portable-x64.zip: drivemount.exe on its own;
    - sha256sums.txt: the three above, as `sha256sum -c` reads it.
    Each has LICENSE.txt and THIRD-PARTY-LICENSES.html (cargo-about, NFR-LIC-04).

    Needs: cargo-wix and cargo-about (`cargo install`), the WiX Toolset 3.14 binaries (-Wix, or the WIX
    environment variable), and the unmodified WinFsp MSI from winfsp.dev (-WinFspMsi, or
    DRIVEMOUNT_WINFSP_MSI). scripts\get-release-tools.ps1 downloads WiX and WinFsp.

    Signing (M2-11, OD-3) goes in this order, as each part is taken into the next: drivemount.exe
    before the MSI and the zip take it, the MSI before the bundle takes it, then the bundle's engine
    (taken out and put back with insignia) and last the bundle. Locally, -Sign does it all with
    scripts\sign.ps1. The release workflow signs with SignPath, which signs on its own servers, so it
    runs the steps one by one (-Step) and signs between them:

        Build    release build, licences, MSI (empties OutDir)
                 -> sign drivemount.exe, and the MSI with the exe inside it
        Bundle   the setup exe, from the MSI
        Detach   takes the engine out of the setup exe, to OutDir\obj\engine.exe
                 -> sign engine.exe
        Attach   puts it back
                 -> sign the setup exe
        Finish   the portable zip and sha256sums.txt; removes what the steps left behind

.PARAMETER Version
    The version the installers carry. The workspace's by default; a test of an upgrade gives another.

.PARAMETER TargetDir
    Cargo's target folder for the release build. Another than `target` leaves a running
    target\release\drivemount.exe alone.

.PARAMETER OutDir
    Where the artefacts go; target\package by default. The Build step empties it first.

.PARAMETER NoBuild
    Use the release exe already built.

.PARAMETER Sign
    Sign the exe, MSI and setup exe with scripts\sign.ps1, which says what it needs. Only with all
    steps.

.PARAMETER Step
    Run one step only (see above); all of them by default.
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$Wix = $(if ($env:WIX) { $env:WIX } else { 'C:\marcin\tools\wix314' }),
    [string]$WinFspMsi = $(if ($env:DRIVEMOUNT_WINFSP_MSI) { $env:DRIVEMOUNT_WINFSP_MSI } else { 'C:\marcin\tools\winfsp\winfsp-2.2.26286.msi' }),
    [string]$TargetDir,
    [string]$OutDir,
    [switch]$NoBuild,
    [switch]$Sign,
    [ValidateSet('All', 'Build', 'Bundle', 'Detach', 'Attach', 'Finish')][string]$Step = 'All'
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

function Invoke-Sign {
    param([string[]]$Files)
    if ($Sign) { & (Join-Path $PSScriptRoot 'sign.ps1') -Path $Files }
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
if ($Sign -and $Step -ne 'All') { throw '-Sign signs between the steps, so it goes with all of them' }

$exe = Join-Path $release 'drivemount.exe'
$msi = Join-Path $out "DriveMount-$Version-x64.msi"
$setup = Join-Path $out "DriveMount-$Version-setup.exe"
$zip = Join-Path $out "DriveMount-$Version-portable-x64.zip"
$obj = Join-Path $out 'obj'
$engine = Join-Path $obj 'engine.exe'

function Assert-There {
    param([string]$File)
    if (-not (Test-Path $File)) { throw "$File is not there; run the steps before this one" }
}


function Invoke-Build {
    if (-not $NoBuild) {
        Invoke-Checked 'release build' { cargo build --release -p drivemount --target-dir $TargetDir --manifest-path (Join-Path $root 'Cargo.toml') }
    }
    if (-not (Test-Path $exe)) { throw "$exe is not there; build without -NoBuild" }
    Invoke-Sign $exe

    if (Test-Path $out) { Remove-Item -Recurse -Force $out }
    New-Item -ItemType Directory -Force $stage | Out-Null

    Invoke-Checked 'third-party licences' {
        cargo about generate --manifest-path (Join-Path $app 'Cargo.toml') -c (Join-Path $root 'about.toml') `
            (Join-Path $root 'packaging\about.hbs') -o (Join-Path $stage 'THIRD-PARTY-LICENSES.html')
    }
    Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')
    # Beside the release exe too, so the About page's button finds it in a local run.
    Copy-Item (Join-Path $stage 'THIRD-PARTY-LICENSES.html') $release

    Invoke-Checked 'MSI' {
        cargo wix --package drivemount --no-build --nocapture `
            --target-bin-dir $release --bin-path $wixBin --install-version $Version --output $msi `
            --compiler-arg "-dStage=$stage" --compiler-arg "-dIcon=$icon" `
            --linker-arg '-sice:ICE57'
        # ICE57 takes the Start menu for a per-user folder, as it does not look at ALLUSERS; in this
        # per-machine package it is the all-users one, and the shortcut's key path is under HKLM.
    }
    Remove-Item (Join-Path $out '*.wixpdb')
    Invoke-Sign $msi
}

function Invoke-Bundle {
    Assert-There $msi
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
    Remove-Item (Join-Path $out '*.wixpdb')
}

# The engine (the bundle's own exe part) is signed on its own first, so the copy Windows keeps for
# uninstalling is signed too; the bundle's signature then covers it and the attached MSIs.
function Invoke-Detach {
    Assert-There $setup
    New-Item -ItemType Directory -Force $obj | Out-Null
    Invoke-Checked 'setup exe: detach the engine' {
        & (Join-Path $wixBin 'insignia.exe') -nologo -ib $setup -o $engine
    }
}

function Invoke-Attach {
    Assert-There $setup
    Assert-There $engine
    Invoke-Checked 'setup exe: attach the engine' {
        & (Join-Path $wixBin 'insignia.exe') -nologo -ab $engine $setup -o $setup
    }
}

function Invoke-Finish {
    foreach ($file in $exe, $msi, $setup) { Assert-There $file }
    $portable = Join-Path $out 'portable'
    if (Test-Path $portable) { Remove-Item -Recurse -Force $portable }
    New-Item -ItemType Directory -Force $portable | Out-Null
    Copy-Item $exe, (Join-Path $stage 'LICENSE.txt'), (Join-Path $stage 'THIRD-PARTY-LICENSES.html') $portable
    if (Test-Path $zip) { Remove-Item $zip }
    Compress-Archive -Path (Join-Path $portable '*') -DestinationPath $zip -CompressionLevel Optimal
    Remove-Item -Recurse -Force $portable
    if (Test-Path $obj) { Remove-Item -Recurse -Force $obj }

    # Two spaces and LF endings, as sha256sum writes them.
    $sums = foreach ($file in $setup, $msi, $zip) {
        '{0}  {1}' -f (Get-FileHash $file -Algorithm SHA256).Hash.ToLowerInvariant(), (Split-Path -Leaf $file)
    }
    [IO.File]::WriteAllText((Join-Path $out 'sha256sums.txt'), (($sums -join "`n") + "`n"))

    Write-Host '== done'
    Get-ChildItem $out -File | Format-Table Name, Length
}

switch ($Step) {
    'Build' { Invoke-Build }
    'Bundle' { Invoke-Bundle }
    'Detach' { Invoke-Detach }
    'Attach' { Invoke-Attach }
    'Finish' { Invoke-Finish }
    'All' {
        Invoke-Build
        Invoke-Bundle
        if ($Sign) {
            Invoke-Detach
            Invoke-Sign $engine
            Invoke-Attach
            Invoke-Sign $setup
        }
        Invoke-Finish
    }
}
