# SPDX-License-Identifier: GPL-3.0-or-later
#
# Makes the SMB share and the two local users that crates/drivemount-smb/tests/local_share.rs maps
# through this machine's own server (\\127.0.0.1, plan M3-05, spike M3-01 §8), or removes them.
# Needs administrator rights.
#
#   smb-test-share.ps1                       make them; write the settings to -EnvFile
#   smb-test-share.ps1 -EnvFile $env:GITHUB_ENV   the same in CI (appends, masks the passwords)
#   smb-test-share.ps1 -Remove               remove the share, the users, the folder and the file
#
# The users are standard users with new random passwords each run. They can log on over the network
# while they exist, so remove them when done on a machine other people can reach.
param(
    [switch]$Remove,
    [string]$EnvFile = (Join-Path (Split-Path $PSScriptRoot -Parent) 'target\smb-test-share.env'),
    [string]$Share = 'drivemount-test',
    [string]$Folder = (Join-Path $env:SystemDrive 'drivemount-smb-test')
)
$ErrorActionPreference = 'Stop'

$users = @('dmsmb1', 'dmsmb2')
$inCi = $env:GITHUB_ENV -and ($EnvFile -eq $env:GITHUB_ENV)

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) { throw 'Run this from an elevated PowerShell (administrator rights).' }

function Remove-TestShare {
    if (Get-SmbShare -Name $Share -ErrorAction SilentlyContinue) { Remove-SmbShare -Name $Share -Force }
    foreach ($user in $users) {
        if (Get-LocalUser -Name $user -ErrorAction SilentlyContinue) { Remove-LocalUser -Name $user }
    }
    if (Test-Path $Folder) { Remove-Item -Recurse -Force $Folder }
}

if ($Remove) {
    Remove-TestShare
    if (-not $inCi -and (Test-Path $EnvFile)) { Remove-Item -Force $EnvFile }
    Write-Host "Removed \\127.0.0.1\$Share, its folder and the users $($users -join ', ')."
    return
}

# A password that meets the default complexity rules: random letters and digits, plus one of each kind.
function New-TestPassword {
    $alphabet = [char[]]'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789'
    $bytes = New-Object byte[] 24
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $random = -join ($bytes | ForEach-Object { $alphabet[$_ % $alphabet.Length] })
    return "Dm7-$random"
}

$server = Get-Service -Name LanmanServer
if ($server.Status -ne 'Running') { Start-Service -Name LanmanServer }

Remove-TestShare
$null = New-Item -ItemType Directory -Path $Folder
$null = New-Item -ItemType Directory -Path (Join-Path $Folder 'sub')
[IO.File]::WriteAllText((Join-Path $Folder 'hello.txt'), "hello`n")

# The signed-in user, for *Current Windows user*. An elevated shell is still that user.
$me = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$settings = [ordered]@{ DRIVEMOUNT_SMB_LOCAL_SHARE = $Share }
$number = 0
foreach ($user in $users) {
    $number++
    $password = New-TestPassword
    if ($inCi) { Write-Host "::add-mask::$password" }
    $secure = ConvertTo-SecureString $password -AsPlainText -Force
    $null = New-LocalUser -Name $user -Password $secure -PasswordNeverExpires -UserMayNotChangePassword `
        -Description 'DriveMount SMB test user (smb-test-share.ps1)'
    # Users, by SID: the group's name is translated.
    Add-LocalGroupMember -SID 'S-1-5-32-545' -Member $user
    $settings["DRIVEMOUNT_SMB_LOCAL_USER$number"] = $user
    $settings["DRIVEMOUNT_SMB_LOCAL_PASS$number"] = $password
}

$grants = @($users | ForEach-Object { "${_}:(OI)(CI)M" }) + "${me}:(OI)(CI)M"
$null = & icacls.exe $Folder /grant @grants
if ($LASTEXITCODE -ne 0) { throw "icacls failed with exit code $LASTEXITCODE" }
$null = New-SmbShare -Name $Share -Path $Folder -FullAccess (@($users | ForEach-Object { "$env:COMPUTERNAME\$_" }) + $me)

$lines = ($settings.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join "`n"
$utf8 = New-Object Text.UTF8Encoding $false
if ($inCi) {
    [IO.File]::AppendAllText($EnvFile, "$lines`n", $utf8)
} else {
    $null = New-Item -ItemType Directory -Force -Path (Split-Path $EnvFile -Parent)
    [IO.File]::WriteAllText($EnvFile, "$lines`n", $utf8)
}
Write-Host "Made \\127.0.0.1\$Share ($Folder) for $($users -join ', ') and $me; settings in $EnvFile."
