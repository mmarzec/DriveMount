# SPDX-License-Identifier: GPL-3.0-or-later
#
# Installs WinFsp with its Developer feature, which building fsm-winfsp needs, on a CI runner.
# Does nothing where it is there already, as on a runner set up by hand. Needs administrator rights.
# Pinned by version and hash: a new WinFsp release means updating both here.

$ErrorActionPreference = 'Stop'

$version = '2.2.26286'
$tag = 'v2.2B5'
$sha256 = '293FFB9BCC0B625C85079FE85FA77F8FE98170ABA51A02AC9C83D58F6C3B0E2F'

$key = Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -ErrorAction SilentlyContinue
if ($key -and (Test-Path (Join-Path $key.InstallDir 'inc\winfsp\winfsp.h'))) {
    Write-Output "WinFsp is already installed: $($key.InstallDir)"
    exit 0
}

$msi = Join-Path $env:TEMP "winfsp-$version.msi"
$url = "https://github.com/winfsp/winfsp/releases/download/$tag/winfsp-$version.msi"
curl.exe --fail --silent --show-error --location --output $msi $url
if ($LASTEXITCODE -ne 0) {
    throw "Could not download $url"
}

$actual = (Get-FileHash $msi -Algorithm SHA256).Hash
if ($actual -ne $sha256) {
    Remove-Item $msi
    throw "$msi has hash $actual, not the expected $sha256"
}

$install = Start-Process msiexec -ArgumentList @('/i', $msi, 'ADDLOCAL=ALL', '/qn') -Wait -PassThru
Remove-Item $msi
if ($install.ExitCode -ne 0) {
    throw "Installing WinFsp failed: msiexec gave $($install.ExitCode)"
}
Write-Output "Installed WinFsp $version"
