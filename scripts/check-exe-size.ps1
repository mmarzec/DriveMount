# SPDX-License-Identifier: GPL-3.0-or-later
#
# NFR-PERF-03: fails when the release fsm.exe is not under 15 MB. Prints the size on every run, so
# growth shows up when it happens. Build it first: cargo build --release -p fsm

param(
    [string]$Path = (Join-Path $PSScriptRoot '..\target\release\fsm.exe'),
    [long]$LimitBytes = 15000000
)

if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    Write-Output "not found: $Path"
    exit 1
}

$size = (Get-Item -LiteralPath $Path).Length
$culture = [Globalization.CultureInfo]::InvariantCulture
$message = [string]::Format($culture, 'fsm.exe: {0} bytes ({1:F2} MB), limit {2} bytes, margin {3} bytes',
    $size, ($size / 1000000), $LimitBytes, ($LimitBytes - $size))
Write-Output $message

if ($size -ge $LimitBytes) {
    Write-Output 'over the size limit (NFR-PERF-03)'
    exit 1
}
exit 0
