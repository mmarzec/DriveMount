# SPDX-License-Identifier: GPL-3.0-or-later
#
# The soak of plan M2-09 (NFR-REL-06): the release drivemount.exe with 5 SFTP mounts on the S-03
# server, a copy loop on each (soak-worker.ps1), the server's traffic dropped with iptables now and
# then (soak-net.sh), the PC put to sleep and woken by a timer now and then, and the app's memory
# logged every minute. At the end the drives must come back, the files on the server must match their
# hashes, killing the app must leave no drive, and soak-report.ps1 writes report.md.
#
#   tests\docker\servers.ps1 up
#   cargo build --release -p drivemount
#   powershell -ExecutionPolicy Bypass -File scripts\soak\soak.ps1            # 4 h
#   powershell -ExecutionPolicy Bypass -File scripts\soak\soak.ps1 -Hours 0.5 -NoSleep
#
# The app runs with APPDATA, LOCALAPPDATA and USERPROFILE in the results folder, so none of the
# user's settings, profiles or known hosts are read or changed. Its single-instance name is the
# real one, so the user's own DriveMount must not be running. Results: target\soak\<start time>.
param(
    [double]$Hours = 4,
    [int]$Mounts = 5,
    [int]$DropEveryMinutes = 20,
    [int]$DropSeconds = 120,
    [int]$SleepEveryMinutes = 40,
    [int]$SleepSeconds = 120,
    [switch]$NoSleep,
    [string]$HostAddress = '',
    [string]$Exe = '',
    [string]$Out = ''
)
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$distro = 'Ubuntu-22.04'
$container = 'drivemount-test-sshd-1'
if (-not $Exe) { $Exe = Join-Path $repo 'target\release\drivemount.exe' }
if (-not $Out) { $Out = Join-Path $repo ('target\soak\' + (Get-Date -Format 'yyyyMMdd-HHmmss')) }
New-Item -ItemType Directory -Force $Out | Out-Null
$Out = (Resolve-Path $Out).Path
$events = Join-Path $Out 'events.log'
# Whatever the console shows, an error that ends the run included.
Start-Transcript -Path (Join-Path $Out 'console.log') -Append | Out-Null
$utf8 = New-Object Text.UTF8Encoding($false)

function Now { (Get-Date).ToUniversalTime().ToString('o') }

function Event([string]$Kind, [string]$Detail = '') {
    $line = "$(Now) $Kind $Detail"
    [IO.File]::AppendAllText($events, "$line`r`n", $utf8)
    Write-Host $line
}

function Wsl-Root {
    $output = & wsl.exe -d $distro -u root -- @args
    if ($LASTEXITCODE -ne 0) { throw "wsl $($args -join ' ') failed: $output" }
    $output
}

# Runs a shell script in the server's container. It goes as base64, which no quoting on the way can
# change; piped in, PowerShell 5.1 puts a BOM in front.
function In-Container([string]$Script) {
    $encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(($Script -replace "`r", '')))
    $output = & wsl.exe -d $distro -u root --exec docker exec $container sh -c "echo $encoded | base64 -d | sh"
    if ($LASTEXITCODE -ne 0) { throw "in the container: $output" }
    $output
}

Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class SoakPower {
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern IntPtr CreateWaitableTimer(IntPtr attributes, bool manualReset, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetWaitableTimer(IntPtr timer, ref long due, int period, IntPtr routine, IntPtr arg, bool resume);
    [DllImport("kernel32.dll")]
    static extern uint WaitForSingleObject(IntPtr handle, uint ms);
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);
    [DllImport("powrprof.dll", SetLastError = true)]
    static extern bool SetSuspendState(bool hibernate, bool force, bool wakeupEventsDisabled);
    [DllImport("kernel32.dll")]
    public static extern uint SetThreadExecutionState(uint flags);

    // Sleeps (S3) with a timer set to wake the PC after `seconds`, and returns once it is awake.
    public static void SleepFor(int seconds) {
        IntPtr timer = CreateWaitableTimer(IntPtr.Zero, true, null);
        if (timer == IntPtr.Zero) throw new Win32Exception();
        try {
            long due = -(long)seconds * 10000000L;
            if (!SetWaitableTimer(timer, ref due, 0, IntPtr.Zero, IntPtr.Zero, true)) throw new Win32Exception();
            if (!SetSuspendState(false, false, false)) throw new Win32Exception();
            WaitForSingleObject(timer, (uint)(seconds + 600) * 1000);
        } finally {
            CloseHandle(timer);
        }
    }
}
'@

# ---- Checks before anything starts. ----
if (-not (Test-Path $Exe)) { throw "No $Exe (cargo build --release -p drivemount)" }
if (Get-Process drivemount -ErrorAction SilentlyContinue) { throw 'DriveMount is running: quit it first' }
if (-not $HostAddress) { $HostAddress = ((Wsl-Root hostname -I) -split '\s+')[0] }
$ip = (Wsl-Root docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' $container).Trim()
if (-not $ip) { throw "$container is not running: tests\docker\servers.ps1 up" }
if (-not $NoSleep) {
    $wake = powercfg /q SCHEME_CURRENT SUB_SLEEP RTCWAKE | Select-String 'Current AC Power Setting Index: 0x00000000'
    if ($wake) { throw 'Wake timers are off in the power plan (Allow wake timers), so the PC would not wake: turn them on, or use -NoSleep' }
}

# ---- A home of its own for the app. ----
$home_ = Join-Path $Out 'home'
$roaming = Join-Path $home_ 'Roaming'
$local = Join-Path $home_ 'Local'
$ssh = Join-Path $home_ '.ssh'
New-Item -ItemType Directory -Force (Join-Path $roaming 'DriveMount'), $local, $ssh | Out-Null

$taken = @([Environment]::GetLogicalDrives() | ForEach-Object { $_.Substring(0, 1).ToUpperInvariant() })
$taken += @(Get-ChildItem 'HKCU:\Network' -ErrorAction SilentlyContinue | ForEach-Object { $_.PSChildName.ToUpperInvariant() })
$letters = @([char[]]('Z'[0]..'D'[0]) | ForEach-Object { [string]$_ } | Where-Object { $taken -notcontains $_ } | Select-Object -First $Mounts)
if ($letters.Count -lt $Mounts) { throw "Only $($letters.Count) free drive letters" }

$key = Join-Path $repo 'tests\docker\keys\id_ed25519'
$config = @"
schema_version = 1

[settings]
start_with_windows = false
close_to_tray      = false
notifications      = false
log_level          = "info"

"@
for ($n = 1; $n -le $Mounts; $n++) {
    $config += @"
[[profiles]]
id             = "$([guid]::NewGuid())"
name           = "Soak $n"
protocol       = "sftp"
host           = "$HostAddress"
port           = 2222
username       = "keyonly"
auth           = "key"
key_path       = '$key'
remember       = false
remote_path    = "~"
drive_letter   = "$($letters[$n - 1])"
mount_on_start = true
auto_reconnect = true

"@
}
[IO.File]::WriteAllText((Join-Path $roaming 'DriveMount\config.toml'), $config, $utf8)
$hostKey = (In-Container 'cat /etc/ssh/ssh_host_ed25519_key.pub') -split '\s+'
[IO.File]::WriteAllText((Join-Path $ssh 'known_hosts'), "[$HostAddress]:2222 $($hostKey[0]) $($hostKey[1])`n", $utf8)
$null = In-Container 'rm -rf /home/keyonly/soak-*'

$settings = [ordered]@{
    hours = $Hours; mounts = $Mounts; letters = $letters; host = $HostAddress; container_ip = $ip
    drop_every_minutes = $DropEveryMinutes; drop_seconds = $DropSeconds
    sleep_every_minutes = $(if ($NoSleep) { 0 } else { $SleepEveryMinutes }); sleep_seconds = $SleepSeconds
    exe = $Exe; start = (Now)
}
[IO.File]::WriteAllText((Join-Path $Out 'soak.json'), ($settings | ConvertTo-Json), $utf8)

# ---- Start. ----
$ES_CONTINUOUS = [uint32]'0x80000000'
$ES_SYSTEM_REQUIRED = [uint32]1
$null = [SoakPower]::SetThreadExecutionState($ES_CONTINUOUS -bor $ES_SYSTEM_REQUIRED)

# Only the app gets the scratch home: wsl.exe reads .wslconfig from USERPROFILE.
$saved = @($env:APPDATA, $env:LOCALAPPDATA, $env:USERPROFILE)
$env:APPDATA = $roaming
$env:LOCALAPPDATA = $local
$env:USERPROFILE = $home_
try { $app = Start-Process -FilePath $Exe -PassThru }
finally { $env:APPDATA, $env:LOCALAPPDATA, $env:USERPROFILE = $saved }
Event 'app-started' "pid=$($app.Id) letters=$($letters -join '')"
$deadline = (Get-Date).AddSeconds(90)
while (@($letters | Where-Object { -not (Test-Path "${_}:\") }).Count) {
    if ((Get-Date) -gt $deadline) { Event 'FAIL' 'not all drives came up in 90 s'; $app.Kill(); throw 'drives did not come up' }
    Start-Sleep -Milliseconds 500
}
Event 'mounted' ($letters -join '')

$stopFile = Join-Path $Out 'stop'
$worker = Join-Path $PSScriptRoot 'soak-worker.ps1'
$jobs = for ($n = 1; $n -le $Mounts; $n++) {
    Start-Job -FilePath $worker -ArgumentList $letters[$n - 1], $n, $Out, $stopFile
}

$net = Join-Path $PSScriptRoot 'soak-net.sh'
$netIn = (& wsl.exe -d $distro -- wslpath -a ($net -replace '\\', '/')).Trim()
$stopIn = (& wsl.exe -d $distro -- wslpath -a ($stopFile -replace '\\', '/')).Trim()
$dropper = Start-Process -FilePath wsl.exe -WindowStyle Hidden -PassThru `
    -ArgumentList '-d', $distro, '-u', 'root', '--', 'sh', $netIn, $ip, ($DropEveryMinutes * 60), $DropSeconds, $stopIn `
    -RedirectStandardOutput (Join-Path $Out 'net.log') -RedirectStandardError (Join-Path $Out 'net-err.log')

# ---- Watch. ----
$memory = Join-Path $Out 'memory.csv'
[IO.File]::WriteAllText($memory, "time,working_set,private_bytes,handles,threads,drives_present`r`n", $utf8)

function Server-Check([string]$When) {
    $script = @'
cd /home/keyonly || exit 0
ok=0
for f in soak-*/*.bin; do
  [ -e "$f" ] || continue
  h=$(sha256sum "$f" 2>/dev/null | cut -d' ' -f1)
  [ -n "$h" ] || continue
  case "$(basename "$f")" in "$h"-*) ok=$((ok+1));; *) echo "MISMATCH $f $h";; esac
done
echo "checked $ok"
'@
    $result = @(In-Container $script)
    $bad = @($result | Where-Object { $_ -like 'MISMATCH*' })
    foreach ($line in $bad) { Event 'SERVER-MISMATCH' $line }
    Event 'server-check' "$When $(($result | Where-Object { $_ -like 'checked*' }) -join '') mismatches=$($bad.Count)"
}

$start = Get-Date
$end = $start.AddHours($Hours)
$nextSample = $start
$nextServerCheck = $start.AddHours(1)
$nextSleep = if ($NoSleep) { [datetime]::MaxValue } else { $start.AddMinutes($SleepEveryMinutes / 2) }
$stuckReported = @{}
$stuckAfter = [TimeSpan]::FromMinutes(5)

while ((Get-Date) -lt $end) {
  # A fault of this script is logged and the run goes on: only the app and the drives may end it.
  try {
    $app.Refresh()
    if ($app.HasExited) { Event 'FAIL' "the app exited with $($app.ExitCode)"; break }
    $now = Get-Date

    if ($now -ge $nextSample) {
        $present = @($letters | Where-Object { [IO.Directory]::Exists("${_}:\") }).Count
        $line = '{0},{1},{2},{3},{4},{5}' -f (Now), $app.WorkingSet64, $app.PrivateMemorySize64, $app.HandleCount, $app.Threads.Count, $present
        [IO.File]::AppendAllText($memory, "$line`r`n", $utf8)
        $nextSample = $now.AddMinutes(1)
    }

    # A call that has not come back in 5 minutes is stuck: no callback may take over the timeout.
    for ($n = 1; $n -le $Mounts; $n++) {
        $beat = Join-Path $Out "w$n.beat"
        # The worker rewrites the file all the time, so a read can find it empty or locked: skip it then.
        try {
            $op, $since = ([IO.File]::ReadAllText($beat)) -split ' ', 2
            $since = [datetime]::Parse($since, $null, [Globalization.DateTimeStyles]::RoundtripKind)
        } catch { continue }
        if ($op -eq 'idle' -or $op -eq 'stopped') { $stuckReported.Remove($n); continue }
        $age = (Get-Date).ToUniversalTime() - $since.ToUniversalTime()
        if ($age -gt $stuckAfter -and -not $stuckReported[$n]) {
            Event 'STUCK' "drive $($letters[$n - 1]): $op since $since"
            $stuckReported[$n] = $true
        }
    }
    $dead = @($jobs | Where-Object { $_.State -ne 'Running' })
    if ($dead.Count) {
        foreach ($job in $dead) { Event 'FAIL' "worker $($job.Id) ended: $($job.State) $((Receive-Job $job 2>&1) -join ' ')" }
        break
    }

    if ($now -ge $nextServerCheck) { Server-Check 'hourly'; $nextServerCheck = $now.AddHours(1) }

    if ($now -ge $nextSleep -and $now.AddSeconds($SleepSeconds + 300) -lt $end) {
        Event 'sleep' "for $SleepSeconds s"
        $before = Get-Date
        try { [SoakPower]::SleepFor($SleepSeconds) } catch { Event 'sleep-failed' $_.Exception.Message }
        Event 'resume' ('after {0:N0} s' -f ((Get-Date) - $before).TotalSeconds)
        $nextSleep = (Get-Date).AddMinutes($SleepEveryMinutes)
        # The minute's sample and the stuck clock both start again from here.
        $nextSample = Get-Date
        continue
    }
    Start-Sleep -Seconds 5
  } catch {
    Event 'harness-error' "$($_.Exception.Message) at line $($_.InvocationInfo.ScriptLineNumber)"
    Start-Sleep -Seconds 5
  }
}

# ---- Stop the load and the drops, then everything must come back. ----
Event 'stopping'
[IO.File]::WriteAllText($stopFile, '', $utf8)
$null = Wait-Job $jobs -Timeout 300
foreach ($job in $jobs) {
    if ($job.State -eq 'Running') { Event 'STUCK' "worker $($job.Id) did not stop in 5 minutes"; Stop-Job $job }
    $output = @(Receive-Job $job 2>&1)
    if ($output.Count) { [IO.File]::AppendAllText((Join-Path $Out 'workers.txt'), ($output -join "`r`n") + "`r`n", $utf8) }
}
if (-not $dropper.WaitForExit(30000)) { $dropper.Kill() }
$null = Wsl-Root sh $netIn clear $ip
Event 'network-restored'

$app.Refresh()
if (-not $app.HasExited) {
    for ($n = 1; $n -le $Mounts; $n++) {
        $letter = $letters[$n - 1]
        $probe = "${letter}:\soak-$n\final-probe.txt"
        $deadline = (Get-Date).AddMinutes(5)
        $back = $false
        while (-not $back -and (Get-Date) -lt $deadline) {
            try {
                [IO.File]::WriteAllText($probe, 'back', $utf8)
                $back = ([IO.File]::ReadAllText($probe) -eq 'back')
                Remove-Item -LiteralPath $probe
            } catch { Start-Sleep -Seconds 5 }
        }
        if ($back) { Event 'drive-back' $letter } else { Event 'STUCK' "drive $letter not usable 5 minutes after the network came back" }
    }
    Server-Check 'final'

    $present = @($letters | Where-Object { [IO.Directory]::Exists("${_}:\") }).Count
    $line = '{0},{1},{2},{3},{4},{5}' -f (Now), $app.WorkingSet64, $app.PrivateMemorySize64, $app.HandleCount, $app.Threads.Count, $present
    [IO.File]::AppendAllText($memory, "$line`r`n", $utf8)

    # The kill test once more, after hours of use (NFR-REL-04).
    & taskkill.exe /F /PID $app.Id | Out-Null
    $app.WaitForExit(10000) | Out-Null
    Start-Sleep -Seconds 2
    $left = @($letters | Where-Object { [IO.Directory]::Exists("${_}:\") })
    if ($left.Count) { Event 'FAIL' "drives left after killing the app: $($left -join '')" } else { Event 'killed' 'no drive left' }
}
$null = In-Container 'rm -rf /home/keyonly/soak-*'
Remove-Job $jobs -Force
Event 'end'

& (Join-Path $PSScriptRoot 'soak-report.ps1') -Out $Out
