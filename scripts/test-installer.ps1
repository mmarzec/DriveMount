# SPDX-License-Identifier: GPL-3.0-or-later
<#
.SYNOPSIS
    Installs, upgrades and uninstalls DriveMount's MSI and setup exe on this PC, checking each step
    (plan M2-10). Run elevated, with DriveMount not installed.

.DESCRIPTION
    Build two versions first, the second higher:

        scripts\package.ps1 -OutDir target\package-old -Version 0.1.0
        scripts\package.ps1 -OutDir target\package-new -Version 0.1.1 -NoBuild

    Then, from an elevated PowerShell:

        scripts\test-installer.ps1 -Old target\package-old -New target\package-new

    Steps:
    1. MSI (old) with STARTWITHWINDOWS=1: files, Start-menu shortcut and its AppUserModelID, Run value.
    2. MSI (new) over it, without the property: one product, new version, Run value kept.
    3. Silent uninstall: program, shortcut, Run value and toast keys gone; settings kept.
    4. Setup exe (new), quiet: WinFsp found and left alone if it is 2.1 or later; DriveMount installed
       and only the setup exe listed in Apps & features.
    5. Setup exe uninstall, quiet: DriveMount gone, WinFsp still there.

    It touches this user's Run value and toast keys, and puts back what was there before. It never
    removes settings (that path asks, or needs REMOVESETTINGS=1: unit tests and the owner's check
    cover it). Logs go to target\installer-test.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string]$Old,
    [Parameter(Mandatory)] [string]$New
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$logs = Join-Path $root 'target\installer-test'
New-Item -ItemType Directory -Force $logs | Out-Null
Start-Transcript -Path (Join-Path $logs 'transcript.txt') -Force | Out-Null

$installDir = Join-Path $env:ProgramFiles 'DriveMount'
$exe = Join-Path $installDir 'drivemount.exe'
$shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\DriveMount.lnk'
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$runValue = "`"$exe`" --tray"
$toastKeys = @(
    'HKCU\Software\Classes\AppUserModelId\DriveMount',
    'HKCU\Software\Classes\CLSID\{F458B6D8-450A-4371-8114-627B7E93EBBC}'
)
$script:failures = 0

function Check {
    param([string]$What, [bool]$Ok)
    if ($Ok) { Write-Host "  ok    $What" } else { Write-Host "  FAIL  $What"; $script:failures++ }
}

function Find-One {
    param([string]$Folder, [string]$Pattern)
    $found = @(Get-ChildItem (Join-Path $root $Folder) -Filter $Pattern)
    if ($found.Count -ne 1) { throw "expected one $Pattern in $Folder, found $($found.Count)" }
    $found[0].FullName
}

function Get-Entries {
    $places = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*',
              'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    @(Get-ItemProperty $places -ErrorAction SilentlyContinue |
        Where-Object { $_.PSObject.Properties['DisplayName'] -and $_.DisplayName -eq 'DriveMount' })
}

function Get-Listed {
    @(Get-Entries | Where-Object { -not ($_.PSObject.Properties['SystemComponent'] -and $_.SystemComponent -eq 1) })
}

function Get-RunValue {
    $values = Get-ItemProperty $runKey -ErrorAction SilentlyContinue
    if ($values -and $values.PSObject.Properties['DriveMount']) { $values.DriveMount }
}

# reg.exe, judged by its exit code alone: under Stop, Windows PowerShell takes its stderr for an error.
function Invoke-Reg {
    $ErrorActionPreference = 'Continue'
    & reg.exe @args 2>&1 | Out-Null
    $LASTEXITCODE
}

function Get-Aumid {
    $folder = (New-Object -ComObject Shell.Application).NameSpace((Split-Path $shortcut))
    $item = $folder.ParseName((Split-Path -Leaf $shortcut))
    if ($item) { $item.ExtendedProperty('System.AppUserModel.ID') }
}

function Invoke-Msi {
    param([string]$What, [string[]]$Arguments)
    $log = Join-Path $logs "$What.log"
    $process = Start-Process msiexec.exe -ArgumentList ($Arguments + @('/qn', '/l*v', "`"$log`"")) -Wait -PassThru
    Check "$What exits 0 (was $($process.ExitCode))" ($process.ExitCode -eq 0)
    $log
}

function Invoke-Setup {
    param([string]$What, [string]$Setup, [string[]]$Arguments)
    $log = Join-Path $logs "$What.log"
    $process = Start-Process $Setup -ArgumentList ($Arguments + @('/quiet', '/norestart', '/log', "`"$log`"")) -Wait -PassThru
    Check "$What exits 0 (was $($process.ExitCode))" ($process.ExitCode -eq 0)
    $log
}

function Get-WinFspVersion {
    $dir = (Get-ItemProperty 'HKLM:\SOFTWARE\WOW6432Node\WinFsp' -ErrorAction SilentlyContinue).InstallDir
    if ($dir) { (Get-Item (Join-Path $dir 'bin\winfsp-x64.dll')).VersionInfo.FileVersion }
}

# --- Before
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $elevated) { throw 'run this from an elevated PowerShell' }
if (@(Get-Entries).Count -ne 0 -or (Test-Path $installDir)) { throw 'DriveMount is installed already; uninstall it first' }

$oldMsi = Find-One $Old 'DriveMount-*-x64.msi'
$newMsi = Find-One $New 'DriveMount-*-x64.msi'
$newSetup = Find-One $New 'DriveMount-*-setup.exe'
$newVersion = [regex]::Match((Split-Path -Leaf $newMsi), '\d+\.\d+\.\d+').Value

$savedRun = Get-RunValue
$savedToasts = Join-Path $logs 'toast-keys'
New-Item -ItemType Directory -Force $savedToasts | Out-Null
$i = 0
foreach ($key in $toastKeys) {
    $i++
    if ((Invoke-Reg query $key) -eq 0) { Invoke-Reg export $key (Join-Path $savedToasts "$i.reg") /y | Out-Null }
}
$settings = Join-Path $env:APPDATA 'DriveMount'
$hadSettings = Test-Path $settings
$winFspBefore = Get-WinFspVersion
Write-Host "WinFsp before: $winFspBefore; Run value before: $savedRun"
# The old value would make step 1's check pass without the MSI.
Remove-ItemProperty $runKey -Name DriveMount -ErrorAction SilentlyContinue

try {
    Write-Host '1. MSI (old), STARTWITHWINDOWS=1'
    Invoke-Msi 'install-old' @('/i', "`"$oldMsi`"", 'STARTWITHWINDOWS=1') | Out-Null
    Check 'drivemount.exe installed' (Test-Path $exe)
    Check 'LICENSE.txt beside it' (Test-Path (Join-Path $installDir 'LICENSE.txt'))
    Check 'THIRD-PARTY-LICENSES.html beside it' (Test-Path (Join-Path $installDir 'THIRD-PARTY-LICENSES.html'))
    Check 'Start-menu shortcut for all users' (Test-Path $shortcut)
    $aumid = Get-Aumid
    Check "shortcut's AppUserModelID is DriveMount (was '$aumid')" ($aumid -eq 'DriveMount')
    Check 'Run value starts the installed exe with --tray' ((Get-RunValue) -eq $runValue)
    Check 'listed once in Apps & features' (@(Get-Listed).Count -eq 1)

    Write-Host '2. MSI (new) over it, no property'
    Invoke-Msi 'upgrade' @('/i', "`"$newMsi`"") | Out-Null
    $entries = @(Get-Entries)
    Check 'one product after the upgrade' ($entries.Count -eq 1)
    Check "its version is $newVersion" ($entries.Count -eq 1 -and $entries[0].DisplayVersion -like "$newVersion*")
    Check 'drivemount.exe still there' (Test-Path $exe)
    Check 'Run value kept' ((Get-RunValue) -eq $runValue)
    Check 'shortcut still there' (Test-Path $shortcut)

    Write-Host '3. Silent uninstall'
    # As the app writes them at each start, so the uninstall step has something to remove.
    foreach ($key in $toastKeys) { Invoke-Reg add $key /f | Out-Null }
    $log = Invoke-Msi 'uninstall' @('/x', "`"$newMsi`"")
    Check 'the uninstall step ran' ((Select-String -Path $log -Pattern 'Doing action: UndoForThisUser' -Quiet) -eq $true)
    Check 'install folder gone' (-not (Test-Path $installDir))
    Check 'shortcut gone' (-not (Test-Path $shortcut))
    Check 'Run value gone' ($null -eq (Get-RunValue))
    foreach ($key in $toastKeys) {
        Check "$key gone" ((Invoke-Reg query $key) -ne 0)
    }
    Check 'settings kept' ((Test-Path $settings) -eq $hadSettings)
    Check 'not listed any more' (@(Get-Entries).Count -eq 0)

    Write-Host '4. Setup exe (new), quiet'
    $log = Invoke-Setup 'setup-install' $newSetup @()
    $text = Get-Content $log -Raw
    if ($winFspBefore) {
        Check 'WinFsp found' ($text -match 'Detected package: WinFsp, state: Present')
        Check 'WinFsp left alone' ($text -match 'Planned package: WinFsp, state: Present, default requested: \w+, ba requested: \w+, execute: None')
    }
    Check 'DriveMount installed' (Test-Path $exe)
    $listed = @(Get-Listed)
    Check 'only the setup exe is listed in Apps & features' ($listed.Count -eq 1 -and $listed[0].PSObject.Properties['BundleVersion'])
    Check 'no Run value with the box unticked' ($null -eq (Get-RunValue))

    Write-Host '5. Setup exe uninstall, quiet'
    Invoke-Setup 'setup-uninstall' $newSetup @('/uninstall') | Out-Null
    Check 'install folder gone' (-not (Test-Path $installDir))
    Check 'not listed any more' (@(Get-Entries).Count -eq 0)
    Check "WinFsp still there ($winFspBefore)" ((Get-WinFspVersion) -eq $winFspBefore)
}
finally {
    Write-Host 'Putting back the Run value and toast keys'
    if ($savedRun) { Set-ItemProperty $runKey -Name DriveMount -Value $savedRun }
    else { Remove-ItemProperty $runKey -Name DriveMount -ErrorAction SilentlyContinue }
    foreach ($key in $toastKeys) { Invoke-Reg delete $key /f | Out-Null }
    Get-ChildItem $savedToasts -Filter *.reg | ForEach-Object { Invoke-Reg import $_.FullName | Out-Null }
    Write-Host ''
    if ($script:failures -eq 0) { Write-Host 'ALL PASSED' } else { Write-Host "$($script:failures) FAILED" }
    Stop-Transcript | Out-Null
}
