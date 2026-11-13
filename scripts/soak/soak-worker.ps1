# SPDX-License-Identifier: GPL-3.0-or-later
#
# One drive's load in the soak (M2-09), started by soak.ps1. In a loop until the stop file appears:
#   write   copy a file from a local pool to <drive>\soak-<n>\<cycle>.part
#   check   hash it back through the drive: a different hash after a write that worked is MISMATCH
#   rename  to <sha256>-<cycle>.bin, so the server check can tell a damaged file by its name
#   list    the folder must show the new name at once (NFR-REL-02), or it is INVISIBLE
#   reread  an older kept file, whose name says its hash: a different one is MISMATCH
#   delete  files past the last 10, which must then be gone from the listing
# Errors while the network is down are expected and logged as ERR. Each step goes to w<n>.csv; the
# step under way and when it started go to w<n>.beat, which soak.ps1 watches for a stuck call.
param(
    [Parameter(Mandatory)][string]$Drive,
    [Parameter(Mandatory)][int]$Number,
    [Parameter(Mandatory)][string]$Out,
    [Parameter(Mandatory)][string]$StopFile
)
$ErrorActionPreference = 'Stop'

$folder = "${Drive}:\soak-$Number"
$csv = Join-Path $Out "w$Number.csv"
$beat = Join-Path $Out "w$Number.beat"
$keep = 10
$utf8 = New-Object Text.UTF8Encoding($false)

function Write-Line([string]$Text) {
    [IO.File]::AppendAllText($csv, "$Text`r`n", $utf8)
}

# The watcher reads this file now and then, so a write can meet its lock: one missed beat is no loss.
function Beat([string]$Text) {
    try { [IO.File]::WriteAllText($beat, $Text, $utf8) } catch { }
}

function Step([int]$Cycle, [string]$Op, [scriptblock]$Body) {
    $started = Get-Date
    Beat "$Op $($started.ToUniversalTime().ToString('o'))"
    $clock = [Diagnostics.Stopwatch]::StartNew()
    try {
        $detail = & $Body
        $result = 'OK'
        if ($detail -is [string] -and ($detail.StartsWith('MISMATCH') -or $detail.StartsWith('INVISIBLE'))) {
            $result, $detail = $detail.Split(' ', 2)
        }
    } catch {
        $result = 'ERR'
        $detail = $_.Exception.Message
    }
    $ms = [int]$clock.Elapsed.TotalMilliseconds
    $text = ("$detail" -replace '[\r\n,]', ' ')
    Write-Line ('{0},{1},{2},{3},{4},{5}' -f $started.ToUniversalTime().ToString('o'), $Cycle, $Op, $result, $ms, $text)
    Beat "idle $((Get-Date).ToUniversalTime().ToString('o'))"
    return $result
}

function Hash([string]$Path) {
    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

# The local pool: the sizes NFR-REL-01 names, short of the GiB, and a few between.
$pool = Join-Path $Out "pool-$Number"
New-Item -ItemType Directory -Force $pool | Out-Null
$sizes = @(0, 1, 4095, 4096, 65537, 1048577, 4194304, 16777216, 300000, 2500000)
$random = New-Object Random($Number)
$files = foreach ($size in $sizes) {
    $path = Join-Path $pool "$size.bin"
    $bytes = New-Object byte[] $size
    $random.NextBytes($bytes)
    [IO.File]::WriteAllBytes($path, $bytes)
    [pscustomobject]@{ Path = $path; Hash = (Hash $path) }
}

Write-Line 'time,cycle,op,result,ms,detail'
$cycle = 0
while (-not (Test-Path $StopFile)) {
    $cycle++
    $source = $files[$cycle % $files.Count]
    $part = Join-Path $folder "$cycle.part"
    $final = Join-Path $folder ('{0}-{1}.bin' -f $source.Hash, $cycle)

    $ok = (Step $cycle 'mkdir' { if (-not (Test-Path -LiteralPath $folder)) { New-Item -ItemType Directory $folder | Out-Null } }) -eq 'OK'
    if ($ok) { $ok = (Step $cycle 'write' { Copy-Item -LiteralPath $source.Path -Destination $part -Force }) -eq 'OK' }
    if ($ok) {
        $ok = (Step $cycle 'check' {
                $got = Hash $part
                if ($got -ne $source.Hash) { "MISMATCH $part is $got, expected $($source.Hash)" }
            }) -eq 'OK'
    }
    if ($ok) { $ok = (Step $cycle 'rename' { Rename-Item -LiteralPath $part -NewName (Split-Path $final -Leaf) }) -eq 'OK' }
    if ($ok) {
        $null = Step $cycle 'list' {
            $names = @(Get-ChildItem -LiteralPath $folder -Name)
            if ($names -notcontains (Split-Path $final -Leaf)) { "INVISIBLE $final not listed" }
        }
    }
    $kept = @()
    $null = Step $cycle 'reread' {
        $script:kept = @(Get-ChildItem -LiteralPath $folder -Filter '*.bin' | Sort-Object { [int]($_.BaseName.Split('-')[1]) })
        if ($kept.Count -gt 1) {
            $old = $kept[$random.Next(0, $kept.Count - 1)]
            $expected = $old.BaseName.Split('-')[0]
            $got = Hash $old.FullName
            if ($got -ne $expected) { "MISMATCH $($old.FullName) is $got" } else { $old.Name }
        }
    }
    if ($kept.Count -gt $keep) {
        $null = Step $cycle 'delete' {
            $gone = $kept[0..($kept.Count - $keep - 1)]
            foreach ($file in $gone) { Remove-Item -LiteralPath $file.FullName }
            # And whatever an interrupted cycle left.
            Get-ChildItem -LiteralPath $folder -Filter '*.part' | Remove-Item -ErrorAction SilentlyContinue
            $names = @(Get-ChildItem -LiteralPath $folder -Name)
            $still = @($gone | Where-Object { $names -contains $_.Name })
            if ($still.Count) { "INVISIBLE deleted but listed: $($still.Name -join ' ')" }
        }
    }
    if (-not $ok) { Start-Sleep -Seconds 1 }
}
Beat "stopped $((Get-Date).ToUniversalTime().ToString('o'))"
