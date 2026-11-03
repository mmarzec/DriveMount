# Scripted checks for spike M0-1 (plan §4, step 4). Run from a normal, unelevated PowerShell.
#   checks.ps1 [-Exe <path>] [-Drive Y] [-WorkDir <dir>] [-SkipLarge]
# Prints one line per check and a FileInfoTimeout operation-count table; exits 1 if any check fails.
param(
    [string]$Exe = (Join-Path $PSScriptRoot 'target\system\release\m0-1-winfsp.exe'),
    [string]$Drive = 'Q',
    [string]$WorkDir = (Join-Path $env:TEMP 'm0-1-work'),
    [switch]$SkipLarge
)
$ErrorActionPreference = 'Stop'
$root = "${Drive}:\"
$script:failures = 0

# ---------------------------------------------------------------- spike process control

function Start-Spike([string[]]$SpikeArgs) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo $Exe, (($SpikeArgs | ForEach-Object { "`"$_`"" }) -join ' ')
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $line = $p.StandardOutput.ReadLine()
    if ($line -notlike 'MOUNTED*') { throw "spike did not mount: $line" }
    $p
}

function Send-Spike($p, [string]$cmd) {
    $p.StandardInput.WriteLine($cmd)
    $p.StandardOutput.ReadLine()
}

function Stop-Spike($p) {
    if (-not $p.HasExited) {
        Send-Spike $p 'quit' | Out-Null
        $p.WaitForExit(10000) | Out-Null
    }
}

function Get-SpikeStats($p) {
    (Send-Spike $p 'stats').Substring(6) | ConvertFrom-Json
}

# ---------------------------------------------------------------- helpers

function Check([string]$Name, [scriptblock]$Body) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    try {
        $detail = & $Body
        '{0,-58} PASS {1,7:N1}s {2}' -f $Name, $sw.Elapsed.TotalSeconds, $detail
    } catch {
        $script:failures++
        '{0,-58} FAIL {1,7:N1}s {2}' -f $Name, $sw.Elapsed.TotalSeconds, $_.Exception.Message
    }
}

function Assert($cond, [string]$msg) { if (-not $cond) { throw $msg } }

function Get-TreeHashes([string]$dir) {
    $h = @{}
    $prefixLen = $dir.TrimEnd('\').Length + 1
    Get-ChildItem $dir -Recurse -File | ForEach-Object {
        $h[$_.FullName.Substring($prefixLen)] = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
    }
    $h
}

function Compare-Trees([string]$a, [string]$b) {
    $ha = Get-TreeHashes $a
    $hb = Get-TreeHashes $b
    Assert ($ha.Count -eq $hb.Count) "file count differs: $($ha.Count) vs $($hb.Count)"
    foreach ($k in $ha.Keys) { Assert ($ha[$k] -eq $hb[$k]) "hash differs: $k" }
    "$($ha.Count) files identical"
}

function Invoke-Robocopy([string]$src, [string]$dst) {
    robocopy $src $dst /MIR /NJH /NJS /NP /NFL /NDL /R:0 /W:0 | Out-Null
    Assert ($LASTEXITCODE -lt 8) "robocopy exit code $LASTEXITCODE"
}

function New-RandomFile([string]$path, [long]$size) {
    $rng = New-Object System.Random ([int]($size % [int]::MaxValue))
    $buf = New-Object byte[] ([Math]::Min($size, 1MB))
    $fs = [IO.File]::Create($path)
    try {
        $left = $size
        while ($left -gt 0) {
            $rng.NextBytes($buf)
            $n = [Math]::Min($left, $buf.Length)
            $fs.Write($buf, 0, $n)
            $left -= $n
        }
    } finally { $fs.Dispose() }
}

function Test-DriveGone {
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-Date) -lt $deadline) {
        if ([Environment]::GetLogicalDrives() -notcontains $root) { return $true }
        Start-Sleep -Milliseconds 200
    }
    $false
}

# ---------------------------------------------------------------- check groups

function Invoke-BasicChecks([string]$mode) {
    Check "[$mode] create, write, read file" {
        [IO.File]::WriteAllText("${root}a.txt", 'alpha')
        Assert ([IO.File]::ReadAllText("${root}a.txt") -eq 'alpha') 'content mismatch'
    }
    Check "[$mode] mkdir nested, list" {
        [IO.Directory]::CreateDirectory("${root}d1\d2\d3") | Out-Null
        [IO.File]::WriteAllText("${root}d1\d2\f.txt", 'x')
        $names = (Get-ChildItem "${root}d1\d2").Name -join ','
        Assert ($names -eq 'd3,f.txt') "listing: $names"
    }
    Check "[$mode] rename file, case-only rename" {
        Rename-Item "${root}a.txt" 'b.txt'
        Assert (-not (Test-Path "${root}a.txt") -and (Test-Path "${root}b.txt")) 'rename failed'
        [IO.File]::Move("${root}b.txt", "${root}B.TXT")
        $name = (Get-ChildItem $root -Filter 'b.txt').Name
        Assert ($name -ceq 'B.TXT') "case not preserved: $name"
        Assert ([IO.File]::ReadAllText("${root}b.txt") -eq 'alpha') 'case-insensitive open failed'
    }
    Check "[$mode] move directory with contents" {
        [IO.Directory]::Move("${root}d1", "${root}moved")
        Assert ([IO.File]::ReadAllText("${root}moved\d2\f.txt") -eq 'x') 'child lost in move'
        Assert (-not (Test-Path "${root}d1")) 'old dir still there'
    }
    Check "[$mode] rename onto existing: refuse, then replace" {
        [IO.File]::WriteAllText("${root}r1.txt", 'one')
        [IO.File]::WriteAllText("${root}r2.txt", 'two')
        $refused = $false
        try { [IO.File]::Move("${root}r1.txt", "${root}r2.txt") } catch { $refused = $true }
        Assert $refused 'move onto existing file was not refused'
        Move-Item "${root}r1.txt" "${root}r2.txt" -Force
        Assert ([IO.File]::ReadAllText("${root}r2.txt") -eq 'one') 'replace failed'
    }
    Check "[$mode] delete non-empty dir refused, recursive ok" {
        $refused = $false
        try { [IO.Directory]::Delete("${root}moved", $false) } catch { $refused = $true }
        Assert $refused 'non-empty directory delete was not refused'
        [IO.Directory]::Delete("${root}moved", $true)
        Assert (-not (Test-Path "${root}moved")) 'recursive delete failed'
    }
    Check "[$mode] overwrite, append, truncate, offset write" {
        $p = "${root}rw.bin"
        [IO.File]::WriteAllBytes($p, [byte[]](1..100))
        [IO.File]::WriteAllBytes($p, [byte[]](1..10))
        Assert ((Get-Item $p).Length -eq 10) 'overwrite length'
        $fs = [IO.File]::Open($p, 'Append'); $fs.Write([byte[]](7, 7), 0, 2); $fs.Dispose()
        Assert ((Get-Item $p).Length -eq 12) 'append length'
        $fs = [IO.File]::Open($p, 'Open'); $fs.Seek(100000, 'Begin') | Out-Null; $fs.WriteByte(9); $fs.SetLength(100001); $fs.Dispose()
        $b = [IO.File]::ReadAllBytes($p)
        Assert ($b.Length -eq 100001 -and $b[100000] -eq 9 -and $b[50000] -eq 0 -and $b[11] -eq 7) 'sparse write content'
        $fs = [IO.File]::Open($p, 'Open'); $fs.SetLength(5); $fs.Dispose()
        Assert ((Get-Item $p).Length -eq 5) 'truncate length'
    }
    Check "[$mode] read-only attribute and timestamps" {
        $p = "${root}attr.txt"
        [IO.File]::WriteAllText($p, 'a')
        Set-ItemProperty $p -Name IsReadOnly -Value $true
        Assert ((Get-Item $p).IsReadOnly) 'read-only not set'
        $refused = $false
        try { [IO.File]::WriteAllText($p, 'b') } catch { $refused = $true }
        Assert $refused 'write to read-only file was not refused'
        Set-ItemProperty $p -Name IsReadOnly -Value $false
        $t = New-Object DateTime 2020, 1, 2, 3, 4, 5, ([DateTimeKind]::Utc)
        [IO.File]::SetLastWriteTimeUtc($p, $t)
        Assert ([IO.File]::GetLastWriteTimeUtc($p) -eq $t) 'mtime not kept'
    }
    Check "[$mode] ReplaceFile with backup" {
        [IO.File]::WriteAllText("${root}new.txt", 'new')
        [IO.File]::WriteAllText("${root}cur.txt", 'cur')
        [IO.File]::Replace("${root}new.txt", "${root}cur.txt", "${root}cur.bak")
        Assert ([IO.File]::ReadAllText("${root}cur.txt") -eq 'new') 'target not replaced'
        Assert ([IO.File]::ReadAllText("${root}cur.bak") -eq 'cur') 'backup wrong'
        Assert (-not (Test-Path "${root}new.txt")) 'source still present'
    }
}

function Invoke-BulkChecks([string]$mode, [int]$count) {
    $src = Join-Path $WorkDir "small$count"
    if (-not (Test-Path $src)) {
        $rng = New-Object System.Random 42
        for ($i = 0; $i -lt $count; $i++) {
            $dir = Join-Path $src ('dir{0:D2}' -f ($i % 10))
            if ($i -lt 10) { [IO.Directory]::CreateDirectory($dir) | Out-Null }
            $buf = New-Object byte[] ($rng.Next(0, 16384))
            $rng.NextBytes($buf)
            [IO.File]::WriteAllBytes((Join-Path $dir ('f{0:D5}.bin' -f $i)), $buf)
        }
    }
    Check "[$mode] robocopy /MIR $count small files in, hashes" {
        Invoke-Robocopy $src "${root}small"
        Compare-Trees $src "${root}small"
    }
    Check "[$mode] robocopy /MIR after add/modify/delete 300" {
        $mut = Join-Path $WorkDir 'mutated'
        Invoke-Robocopy $src $mut
        $files = Get-ChildItem $mut -Recurse -File | Select-Object -First 200
        $files | Select-Object -First 100 | Remove-Item
        $files | Select-Object -Skip 100 | ForEach-Object { [IO.File]::AppendAllText($_.FullName, 'changed') }
        1..100 | ForEach-Object { [IO.File]::WriteAllText((Join-Path $mut "dir00\added$_.txt"), "added $_") }
        Invoke-Robocopy $mut "${root}small"
        Compare-Trees $mut "${root}small"
    }
    Check "[$mode] robocopy $count small files out, hashes" {
        $out = Join-Path $WorkDir 'small-out'
        Invoke-Robocopy "${root}small" $out
        Compare-Trees "${root}small" $out
    }
    Remove-Item (Join-Path $WorkDir 'mutated'), (Join-Path $WorkDir 'small-out') -Recurse -Force
    [IO.Directory]::Delete("${root}small", $true)
}

function Invoke-LargeCheck([string]$mode) {
    $big = Join-Path $WorkDir 'big.bin'
    if (-not (Test-Path $big)) { New-RandomFile $big 1GB }
    $srcHash = (Get-FileHash $big -Algorithm SHA256).Hash
    Check "[$mode] 1 GiB copy in (CopyFile), hash" {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        [IO.File]::Copy($big, "${root}big.bin")
        $mbps = 1024 / $sw.Elapsed.TotalSeconds
        Assert ((Get-FileHash "${root}big.bin" -Algorithm SHA256).Hash -eq $srcHash) 'hash mismatch'
        '{0:N0} MiB/s' -f $mbps
    }
    Check "[$mode] 1 GiB copy out (CopyFile), hash" {
        $out = Join-Path $WorkDir 'big-out.bin'
        $sw = [Diagnostics.Stopwatch]::StartNew()
        [IO.File]::Copy("${root}big.bin", $out, $true)
        $mbps = 1024 / $sw.Elapsed.TotalSeconds
        Assert ((Get-FileHash $out -Algorithm SHA256).Hash -eq $srcHash) 'hash mismatch'
        Remove-Item $out
        '{0:N0} MiB/s' -f $mbps
    }
    Remove-Item "${root}big.bin"
}

function Invoke-KillCheck([string]$mode, [string[]]$spikeArgs) {
    Check "[$mode] kill -9 while mounted: letter gone (NFR-REL-04)" {
        $p = Start-Spike $spikeArgs
        [IO.File]::WriteAllText("${root}open.txt", 'x')
        $fs = [IO.File]::Open("${root}open.txt", 'Open')   # keep a handle open across the kill
        Stop-Process -Id $p.Id -Force
        $gone = Test-DriveGone
        $fs.Dispose()
        Assert $gone "$root still in GetLogicalDrives after 10 s"
        'letter released'
    }
}

function Invoke-TimeoutProbe([int]$timeout, [string[]]$baseArgs) {
    $p = Start-Spike ($baseArgs + @('--file-info-timeout', "$timeout"))
    try {
        [IO.Directory]::CreateDirectory("${root}probe") | Out-Null
        1..200 | ForEach-Object { [IO.File]::WriteAllText("${root}probe\f$_.txt", "$_") }
        Send-Spike $p 'reset' | Out-Null
        # Explorer-like workload: list, stat every entry, open one file; five rounds 200 ms apart.
        for ($round = 0; $round -lt 5; $round++) {
            $items = Get-ChildItem "${root}probe"
            $items | ForEach-Object { [IO.File]::GetAttributes($_.FullName) | Out-Null; $_.Refresh() }
            [IO.File]::ReadAllText("${root}probe\f1.txt") | Out-Null
            Start-Sleep -Milliseconds 200
        }
        $s = Get-SpikeStats $p
        [pscustomobject]@{
            FileInfoTimeout = $timeout
            GetSecurityByName = $s.get_security_by_name
            Open = $s.open
            GetFileInfo = $s.get_file_info
            ReadDirectory = $s.read_directory
            Read = $s.read
            Total = ($s.PSObject.Properties.Value | Measure-Object -Sum).Sum
        }
    } finally { Stop-Spike $p }
}

# ---------------------------------------------------------------- main

Assert (Test-Path $Exe) "spike exe not found: $Exe"
Assert ([Environment]::GetLogicalDrives() -notcontains $root) "$root is already in use"
# GetLogicalDrives misses remembered-but-disconnected mappings (net use); never mount over those.
Assert (-not (Test-Path "HKCU:\Network\$Drive")) "${Drive}: is a remembered network mapping"
$elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
"Elevated: $elevated   Exe: $Exe"
New-Item -ItemType Directory -Force $WorkDir | Out-Null

foreach ($mode in 'network', 'disk') {
    $spikeArgs = @('--mount', "${Drive}:")
    if ($mode -eq 'network') { $spikeArgs += @('--prefix', '\fsm\spike') }
    "--- $mode mode ($($spikeArgs -join ' '))"
    $p = Start-Spike $spikeArgs
    try {
        Check "[$mode] drive type" {
            $type = ([IO.DriveInfo]::new($Drive)).DriveType
            $want = if ($mode -eq 'network') { 'Network' } else { 'Fixed' }
            Assert ("$type" -eq $want) "DriveType $type, want $want"
            "$type"
        }
        Invoke-BasicChecks $mode
        if ($mode -eq 'network') {
            Invoke-BulkChecks $mode 10000
            if (-not $SkipLarge) { Invoke-LargeCheck $mode }
        } else {
            Invoke-BulkChecks $mode 1000
        }
    } finally { Stop-Spike $p }
    Check "[$mode] quit unmounts" { Assert (Test-DriveGone) 'letter still present'; 'ok' }
    Invoke-KillCheck $mode $spikeArgs
}

'--- FileInfoTimeout probe (network mode): operation counts for the same workload'
$probe = foreach ($t in 0, 1000, 5000) { Invoke-TimeoutProbe $t @('--mount', "${Drive}:", '--prefix', '\fsm\spike') }
$probe | Format-Table -AutoSize | Out-String -Width 200

"Failures: $script:failures"
if ($script:failures -gt 0) { exit 1 }
