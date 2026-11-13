# SPDX-License-Identifier: GPL-3.0-or-later
#
# Reads a soak's results folder (soak.ps1) and writes report.md beside them, with the verdict of
# NFR-REL-06 and plan M2-09: no data mismatch, no stuck state, working-set growth under 10%.
#   powershell -ExecutionPolicy Bypass -File scripts\soak\soak-report.ps1 -Out target\soak\<folder>
param([Parameter(Mandatory)][string]$Out)
$ErrorActionPreference = 'Stop'
$Out = (Resolve-Path $Out).Path
$utf8 = New-Object Text.UTF8Encoding($false)
$inv = [Globalization.CultureInfo]::InvariantCulture
# Numbers in the report the same on every machine: 1.5, not 1,5.
[Threading.Thread]::CurrentThread.CurrentCulture = $inv

function Time([string]$Text) { [datetime]::Parse($Text, $inv, [Globalization.DateTimeStyles]::RoundtripKind).ToUniversalTime() }
function Median([double[]]$Values) {
    if (-not $Values.Count) { return [double]::NaN }
    $sorted = $Values | Sort-Object
    $sorted[[int][math]::Floor($sorted.Count / 2)]
}
function MiB([double]$Bytes) { '{0:N1} MiB' -f ($Bytes / 1MB) }

$settings = Get-Content (Join-Path $Out 'soak.json') -Raw | ConvertFrom-Json
$start = Time $settings.start
$events = @(Get-Content (Join-Path $Out 'events.log') | ForEach-Object {
        $time, $kind, $detail = $_ -split ' ', 3
        [pscustomobject]@{ Time = (Time $time); Kind = $kind; Detail = $detail }
    })
$end = ($events | Select-Object -Last 1).Time

# Windows where errors are expected: a drop, or a sleep, and 3 minutes after either for reconnecting.
$grace = [TimeSpan]::FromMinutes(3)
$windows = New-Object Collections.ArrayList
$netLog = Join-Path $Out 'net.log'
$drops = 0
if (Test-Path $netLog) {
    $from = $null
    foreach ($line in Get-Content $netLog) {
        $time, $what = $line -split ' ', 2
        if ($what -eq 'drop') { $from = Time $time; $drops++ }
        elseif ($what -like 'restore*' -and $from) { $null = $windows.Add(@($from, (Time $time) + $grace)); $from = $null }
    }
    if ($from) { $null = $windows.Add(@($from, $end)) }
}
$sleeps = 0
$from = $null
foreach ($event in $events) {
    if ($event.Kind -eq 'sleep') { $from = $event.Time; $sleeps++ }
    elseif ($event.Kind -eq 'resume' -and $from) { $null = $windows.Add(@($from, $event.Time + $grace)); $from = $null }
}
# Stopping: the network comes back, the drives catch up.
$stopping = ($events | Where-Object Kind -eq 'stopping' | Select-Object -First 1).Time
if ($stopping) { $null = $windows.Add(@($stopping, $end)) }
function Expected([datetime]$Time) {
    foreach ($window in $windows) { if ($Time -ge $window[0] -and $Time -le $window[1]) { return $true } }
    $false
}

# ---- The drives' load. ----
$failures = New-Object Collections.ArrayList
$driveRows = @()
$unexplained = @()
for ($n = 1; $n -le $settings.mounts; $n++) {
    $file = Join-Path $Out "w$n.csv"
    if (-not (Test-Path $file)) { $null = $failures.Add("drive $n wrote no log"); continue }
    $rows = @(Import-Csv $file)
    $ok = @($rows | Where-Object result -eq 'OK')
    $err = @($rows | Where-Object result -eq 'ERR')
    $bad = @($rows | Where-Object { $_.result -eq 'MISMATCH' -or $_.result -eq 'INVISIBLE' })
    foreach ($row in $bad) { $null = $failures.Add("drive ${n}: $($row.result) at $($row.time), $($row.op): $($row.detail)") }
    $odd = @($err | Where-Object { -not (Expected (Time $_.time)) })
    $unexplained += $odd | ForEach-Object { "drive $n, $($_.time), $($_.op): $($_.detail)" }
    $gap = [TimeSpan]::Zero
    $gapAt = ''
    $last = $start
    foreach ($row in $ok) {
        $t = Time $row.time
        if ($t - $last -gt $gap) { $gap = $t - $last; $gapAt = $last.ToString('HH:mm:ss') }
        $last = $t
    }
    $slowest = ($rows | Sort-Object { [int]$_.ms } -Descending | Select-Object -First 1)
    $driveRows += '| {0} ({1}:) | {2} | {3} | {4} | {5} | {6} | {7:N0} s from {8} | {9:N1} s ({10}) |' -f $n, $settings.letters[$n - 1],
        ($rows | Select-Object -Last 1).cycle, $ok.Count, $err.Count, $odd.Count, $bad.Count, $gap.TotalSeconds, $gapAt,
        ([int]$slowest.ms / 1000), $slowest.op
}

foreach ($event in $events | Where-Object { 'STUCK', 'FAIL', 'SERVER-MISMATCH' -contains $_.Kind }) {
    $null = $failures.Add("$($event.Kind) at $($event.Time.ToString('o')): $($event.Detail)")
}
$serverChecks = @($events | Where-Object Kind -eq 'server-check')
if (-not ($serverChecks | Where-Object { $_.Detail -like 'final*' })) { $null = $failures.Add('no final server check') }

# ---- Memory. ----
$memory = @(Import-Csv (Join-Path $Out 'memory.csv') | ForEach-Object {
        [pscustomobject]@{
            Minutes = ((Time $_.time) - $start).TotalMinutes
            WorkingSet = [double]$_.working_set; Private = [double]$_.private_bytes; Handles = [double]$_.handles; Threads = [double]$_.threads
        }
    })
$total = ($memory | Select-Object -Last 1).Minutes
# After a warm-up: minutes 30 to 60 of a run of 2 hours or more, else its second tenth; against its last
# 30 minutes, or its last tenth.
if ($total -ge 120) { $baseFrom, $baseTo, $finalFrom = 30, 60, ($total - 30) }
else { $baseFrom, $baseTo, $finalFrom = ($total * 0.1), ($total * 0.2), ($total * 0.9) }
$base = @($memory | Where-Object { $_.Minutes -ge $baseFrom -and $_.Minutes -lt $baseTo })
$final = @($memory | Where-Object { $_.Minutes -ge $finalFrom })
$growth = @{}
$memoryRows = foreach ($measure in 'WorkingSet', 'Private', 'Handles', 'Threads') {
    $b = Median ($base | ForEach-Object { $_.$measure })
    $f = Median ($final | ForEach-Object { $_.$measure })
    $max = ($memory | Measure-Object $measure -Maximum).Maximum
    $growth[$measure] = if ($b) { 100 * ($f - $b) / $b } else { [double]::NaN }
    if ($measure -in 'WorkingSet', 'Private') { '| {0} | {1} | {2} | {3} | {4:N1}% |' -f $measure, (MiB $b), (MiB $f), (MiB $max), $growth[$measure] }
    else { '| {0} | {1:N0} | {2:N0} | {3:N0} | {4:N1}% |' -f $measure, $b, $f, $max, $growth[$measure] }
}
# Private bytes after the warm-up, as a least-squares line: a leak climbs steadily, buffers come and go.
$after = @($memory | Where-Object { $_.Minutes -ge $baseFrom })
$slope = [double]::NaN
if ($after.Count -ge 3) {
    $mx = ($after | Measure-Object Minutes -Average).Average
    $my = ($after | Measure-Object Private -Average).Average
    $sxy = 0.0; $sxx = 0.0
    foreach ($m in $after) { $sxy += ($m.Minutes - $mx) * ($m.Private - $my); $sxx += ($m.Minutes - $mx) * ($m.Minutes - $mx) }
    if ($sxx) { $slope = 60 * $sxy / $sxx / 1MB }
}
if (-not ($growth.WorkingSet -lt 10)) { $null = $failures.Add(('working set grew {0:N1}% (limit 10%)' -f $growth.WorkingSet)) }

# ---- The app's own log. ----
$logs = Get-ChildItem (Join-Path $Out 'home\Local\DriveMount\logs') -Filter 'drivemount.log*' -ErrorAction SilentlyContinue
$appLog = @($logs | ForEach-Object { Get-Content $_.FullName })
$reconnected = @($appLog | Select-String 'manager::actor: reconnected').Count
$reconnecting = @($appLog | Select-String 'state=Reconnecting').Count
$stopped = @($appLog | Select-String 'reconnecting stopped').Count
$errorState = @($appLog | Select-String 'state=Error').Count
$panics = @($appLog | Select-String 'panicked').Count
if ($stopped -or $errorState) { $null = $failures.Add("the app gave up on a drive: $stopped 'reconnecting stopped', $errorState Error states") }
if ($panics) { $null = $failures.Add("$panics panics in the app's log") }

# ---- Report. ----
$verdict = if ($failures.Count) { 'FAIL' } else { 'PASS' }
$report = @"
# Soak report: $verdict

| | |
|---|---|
| Started | $($start.ToString('yyyy-MM-dd HH:mm')) UTC |
| Ran for | $('{0:N2}' -f ($end - $start).TotalHours) h (asked: $($settings.hours) h) |
| Mounts | $($settings.mounts), on $($settings.letters -join ', '), to $($settings.host):2222 as ``keyonly`` |
| Network drops | $drops, every $($settings.drop_every_minutes) min for $($settings.drop_seconds) s (iptables DROP on the container) |
| Sleeps | $sleeps, every $($settings.sleep_every_minutes) min for $($settings.sleep_seconds) s (S3, woken by a timer) |
| App log | $reconnecting times *Reconnecting*, $reconnected reconnects, $stopped gave up, $panics panics |
| Server checks | $(($serverChecks | ForEach-Object { $_.Detail }) -join '; ') |

## Verdict

$(if ($failures.Count) { ($failures | ForEach-Object { "- $_" }) -join "`n" } else { 'No data mismatch, no stuck state, working set growth under 10%.' })

## Drives

Each cycle writes a file, hashes it back, renames it, lists the folder, rereads an older file and deletes old ones. Errors are expected during a drop or a sleep and for 3 minutes after; "unexplained" are those outside.

| Drive | Cycles | OK steps | Errors | Unexplained | Mismatch / invisible | Longest without success | Slowest step |
|---|---|---|---|---|---|---|---|
$($driveRows -join "`n")

## Memory

Baseline: median of minutes $('{0:N0}' -f $baseFrom) to $('{0:N0}' -f $baseTo) ($($base.Count) samples); final: median from minute $('{0:N0}' -f $finalFrom) ($($final.Count) samples). Private bytes from minute $('{0:N0}' -f $baseFrom) on, fitted to a line: $('{0:N1}' -f $slope) MiB an hour.

| Measure | Baseline | Final | Max | Growth |
|---|---|---|---|---|
$($memoryRows -join "`n")

## Unexplained errors

$(if ($unexplained.Count) { ($unexplained | Select-Object -First 50 | ForEach-Object { "- $_" }) -join "`n" } else { 'None.' })

## Events

``````
$((Get-Content (Join-Path $Out 'events.log')) -join "`n")
``````
"@
[IO.File]::WriteAllText((Join-Path $Out 'report.md'), $report, $utf8)
Write-Host "$verdict - $(Join-Path $Out 'report.md')"
