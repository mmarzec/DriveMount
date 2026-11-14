# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
    Downloads what scripts\package.ps1 needs besides cargo tools (plan M2-11): the WiX Toolset 3.14.1
    binaries and the unmodified WinFsp MSI the setup exe carries.

.DESCRIPTION
    Each download is pinned by version and SHA-256 and is refused on any other hash: a new release of
    one of them means updating both here. On GitHub Actions the paths are also written to GITHUB_ENV
    as WIX and DRIVEMOUNT_WINFSP_MSI.

.PARAMETER Into
    The folder the tools go in; created if missing. A download already there with the right hash is
    kept.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Into
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$wixUrl = 'https://github.com/wixtoolset/wix3/releases/download/wix3141rtm/wix314-binaries.zip'
$wixSha256 = '6AC824E1642D6F7277D0ED7EA09411A508F6116BA6FAE0AA5F2C7DAA2FF43D31'
# The same WinFsp as scripts\install-winfsp.ps1.
$winfspVersion = '2.2.26286'
$winfspUrl = "https://github.com/winfsp/winfsp/releases/download/v2.2B5/winfsp-$winfspVersion.msi"
$winfspSha256 = '293FFB9BCC0B625C85079FE85FA77F8FE98170ABA51A02AC9C83D58F6C3B0E2F'

New-Item -ItemType Directory -Force $Into | Out-Null
$Into = (Resolve-Path $Into).Path

function Get-Pinned {
    param([string]$Url, [string]$Sha256, [string]$File)
    $path = Join-Path $Into $File
    if ((Test-Path $path) -and (Get-FileHash $path -Algorithm SHA256).Hash -eq $Sha256) { return $path }
    curl.exe --fail --silent --show-error --location --output $path $Url
    if ($LASTEXITCODE -ne 0) { throw "Could not download $Url" }
    $actual = (Get-FileHash $path -Algorithm SHA256).Hash
    if ($actual -ne $Sha256) {
        Remove-Item $path
        throw "$Url has hash $actual, not the expected $Sha256"
    }
    $path
}

$wixZip = Get-Pinned $wixUrl $wixSha256 'wix314-binaries.zip'
$wix = Join-Path $Into 'wix314'
if (Test-Path $wix) { Remove-Item -Recurse -Force $wix }
Expand-Archive -Path $wixZip -DestinationPath $wix

$found = [ordered]@{
    WIX = $wix
    DRIVEMOUNT_WINFSP_MSI = Get-Pinned $winfspUrl $winfspSha256 "winfsp-$winfspVersion.msi"
}
foreach ($name in $found.Keys) {
    Write-Output "$name=$($found[$name])"
    if ($env:GITHUB_ENV) { Add-Content -Path $env:GITHUB_ENV -Value "$name=$($found[$name])" -Encoding ascii }
}
