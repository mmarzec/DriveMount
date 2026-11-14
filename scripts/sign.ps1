# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
    Authenticode-signs files with a certificate in the Windows store, for a local run of
    scripts\package.ps1 -Sign (plan M2-11).

.DESCRIPTION
    Releases are signed by SignPath in the release workflow (OD-3), not by this script. It is here to
    try the signing order of package.ps1 locally, with a self-signed test certificate:

        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=DriveMount test' `
            -CertStoreLocation Cert:\CurrentUser\My -NotAfter (Get-Date).AddDays(7)
        $env:DRIVEMOUNT_SIGN_THUMBPRINT = $cert.Thumbprint
        $env:DRIVEMOUNT_SIGN_UNTRUSTED = '1'
        scripts\package.ps1 -Sign

    DRIVEMOUNT_SIGN_THUMBPRINT is the certificate's SHA-1 thumbprint (CurrentUser\My or
    LocalMachine\My). Every signature is SHA-256 and timestamped (DRIVEMOUNT_SIGN_TIMESTAMP, DigiCert's
    server by default). The script fails unless the signature is valid and timestamped;
    DRIVEMOUNT_SIGN_UNTRUSTED=1 accepts a chain that does not reach a trusted root, and nothing else.

    signtool is SIGNTOOL, or the newest x64 one in the Windows SDK.

.PARAMETER Path
    The files to sign, in place.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string[]]$Path
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$signtool = $env:SIGNTOOL
if (-not $signtool) {
    $signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue |
        Sort-Object { [version]$_.Directory.Parent.Name } | Select-Object -Last 1 -ExpandProperty FullName
}
if (-not $signtool -or -not (Test-Path $signtool)) { throw 'signtool.exe not found (install the Windows SDK or set SIGNTOOL)' }

$thumbprint = $env:DRIVEMOUNT_SIGN_THUMBPRINT
if (-not $thumbprint) { throw 'Nothing to sign with: set DRIVEMOUNT_SIGN_THUMBPRINT' }
$how = @('/sha1', $thumbprint)
# The machine store when the certificate is only there; signtool looks in the user's by default.
if (-not (Test-Path "Cert:\CurrentUser\My\$thumbprint")) { $how += '/sm' }
$timestamp = if ($env:DRIVEMOUNT_SIGN_TIMESTAMP) { $env:DRIVEMOUNT_SIGN_TIMESTAMP } else { 'http://timestamp.digicert.com' }

foreach ($file in $Path) {
    if (-not (Test-Path $file)) { throw "$file is not there" }
    $file = (Resolve-Path $file).Path
    Write-Host "== sign $(Split-Path -Leaf $file)"
    # A timestamp server that is busy fails now and then; signing again is safe.
    for ($try = 1; ; $try++) {
        $ErrorActionPreference = 'Continue'
        & $signtool sign /fd SHA256 /tr $timestamp /td SHA256 /d DriveMount `
            /du 'https://github.com/mmarzec/DriveMount' @how $file
        $ErrorActionPreference = 'Stop'
        if ($LASTEXITCODE -eq 0) { break }
        if ($try -ge 3) { throw "signing $file failed (exit code $LASTEXITCODE)" }
        Start-Sleep -Seconds (10 * $try)
    }

    $signature = Get-AuthenticodeSignature $file
    if (-not $signature.SignerCertificate) { throw "$file has no signature after signing" }
    if (-not $signature.TimeStamperCertificate) { throw "$file's signature has no timestamp" }
    $trusted = $signature.Status -eq 'Valid'
    $test = $env:DRIVEMOUNT_SIGN_UNTRUSTED -eq '1' -and $signature.Status -in 'UnknownError', 'NotTrusted'
    if (-not ($trusted -or $test)) {
        throw "$file's signature is $($signature.Status): $($signature.StatusMessage)"
    }
    Write-Host "   $($signature.Status), $($signature.SignerCertificate.Subject), timestamped by $($signature.TimeStamperCertificate.Subject)"
}
