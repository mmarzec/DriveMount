# SPDX-License-Identifier: GPL-3.0-or-later
#
# Fetches winfsp-tests, which the WinFsp installer does not include, into target\winfsp-tests.
# Pinned by version and hash: a new WinFsp release means updating both here.

$ErrorActionPreference = 'Stop'

$version = '2.2.26286'
$tag = 'v2.2B5'
$sha256 = '4F5CC6CA8E3CE967835E6DD7EA586C0F8B1473B3A4427C1026F496A9EA8EA755'

$directory = Join-Path $PSScriptRoot '..\target\winfsp-tests'
$program = Join-Path $directory 'winfsp-tests-x64.exe'
if (Test-Path $program) {
    Write-Output "Already there: $program"
    exit 0
}

New-Item -ItemType Directory -Force $directory | Out-Null
$zip = Join-Path $directory "winfsp-tests-$version.zip"
$url = "https://github.com/winfsp/winfsp/releases/download/$tag/winfsp-tests-$version.zip"
curl.exe --fail --silent --show-error --location --output $zip $url
if ($LASTEXITCODE -ne 0) {
    throw "Could not download $url"
}

$actual = (Get-FileHash $zip -Algorithm SHA256).Hash
if ($actual -ne $sha256) {
    Remove-Item $zip
    throw "$zip has hash $actual, not the expected $sha256"
}

Expand-Archive -Force $zip $directory
Remove-Item $zip
Write-Output "Fetched: $program"
