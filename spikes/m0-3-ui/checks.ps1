# Spike M0-3 checks. Windows PowerShell 5.1, unelevated. Build first: cargo build --release
#
#   checks.ps1 size        exe size
#   checks.ps1 start       start-up times: warm, cold exe (unbuffered copy) and --tray
#   checks.ps1 behaviour   close-to-tray, show/hide, power and session messages, screenshot
#   checks.ps1 memory      working set and CPU after 60 s, window visible and hidden
#   checks.ps1 idle        idle CPU with the window visible: 3 x 60 s after a settle
#   checks.ps1 toast       toast with no shortcut, a registry AUMID, a shortcut, and both
#   checks.ps1 toast-wait  toast with a shortcut, at growing delays after creating the shortcut
#   checks.ps1 h05-start   register the AUMID and start the spike for the manual checks (H-05)
#   checks.ps1 h05-log     show the H-05 log
#   checks.ps1 cleanup     remove the shortcut, registry keys and any running spike
#
param([Parameter(Position = 0)][string]$Action = 'all')

$ErrorActionPreference = 'Stop'
$Exe = Join-Path $PSScriptRoot 'target\release\m0-3-ui.exe'
$Results = Join-Path $PSScriptRoot 'results'
$RunDir = Join-Path $env:TEMP 'fsm-m0-3'
$Report = Join-Path $Results 'checks.txt'
$Inv = [Globalization.CultureInfo]::InvariantCulture
$HiddenClass = 'FsmSpikeHidden'
$Title = 'FS Manager (M0-3 spike)'
$BaseAumid = 'FsManager.M03Spike'
$Passed = 0
$Failed = 0

New-Item -ItemType Directory -Force $Results, $RunDir | Out-Null

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class W {
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
    [DllImport("user32.dll")] public static extern IntPtr SendMessageTimeout(IntPtr h, uint m, IntPtr w, IntPtr l, uint flags, uint ms, out IntPtr result);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    public struct RECT { public int L, T, R, B; }
    // Saves the screen's bottom-right corner, where toasts appear.
    public static void ShotCorner(int right, int bottom, string path) {
        int w = 420, h = 260;
        using (var bmp = new Bitmap(w, h)) {
            using (var g = Graphics.FromImage(bmp)) g.CopyFromScreen(right - w, bottom - h, 0, 0, new Size(w, h));
            bmp.Save(path, ImageFormat.Png);
        }
    }
    // Saves the window as a PNG. Returns the number of different colours in a sample of it.
    public static int Shot(IntPtr h, string path) {
        RECT r; GetWindowRect(h, out r);
        using (var bmp = new Bitmap(r.R - r.L, r.B - r.T)) {
            using (var g = Graphics.FromImage(bmp)) {
                IntPtr dc = g.GetHdc();
                PrintWindow(h, dc, 2); // PW_RENDERFULLCONTENT
                g.ReleaseHdc(dc);
            }
            bmp.Save(path, ImageFormat.Png);
            var seen = new System.Collections.Generic.HashSet<int>();
            for (int y = 0; y < bmp.Height; y += 3)
                for (int x = 0; x < bmp.Width; x += 3)
                    seen.Add(bmp.GetPixel(x, y).ToArgb());
            return seen.Count;
        }
    }
}
'@
[W]::SetProcessDPIAware() | Out-Null

$WM_APP = 0x8000
$CMD_SHOW = 0; $CMD_HIDE = 1; $CMD_TOAST = 2; $CMD_QUIT = 3; $CMD_MEMORY = 4; $CMD_HISTORY = 5; $CMD_TOGGLE = 6

function Say([string]$Text) {
    Write-Host $Text
    $Text | Out-File -Encoding utf8 -Append $Report
}

function Check([string]$Name, [bool]$Ok, [string]$Detail = '') {
    if ($Ok) { $script:Passed++; $tag = 'PASS' } else { $script:Failed++; $tag = 'FAIL' }
    Say ("{0}  {1}  {2}" -f $tag, $Name, $Detail)
}

function Num([double]$Value, [string]$Format = 'F1') { $Value.ToString($Format, $Inv) }

function Stop-Spike {
    Get-Process m0-3-ui -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 200
}

function Start-Spike([string]$LogName, [string[]]$SpikeArgs = @(), [string]$Path = $Exe) {
    $log = Join-Path $RunDir $LogName
    Remove-Item $log -ErrorAction SilentlyContinue
    $all = @('--log', "`"$log`"") + $SpikeArgs
    $proc = Start-Process -FilePath $Path -ArgumentList $all -PassThru
    [pscustomobject]@{ Proc = $proc; Log = $log }
}

function Read-Log($Run) {
    if (Test-Path $Run.Log) { Get-Content $Run.Log } else { @() }
}

# Waits for a log line matching the pattern and returns it, or $null on timeout.
function Wait-Line($Run, [string]$Pattern, [int]$TimeoutMs = 5000, [int]$Skip = 0) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $TimeoutMs) {
        $hit = Read-Log $Run | Select-Object -Skip $Skip | Where-Object { $_ -match $Pattern } | Select-Object -First 1
        if ($hit) { return $hit }
        Start-Sleep -Milliseconds 20
    }
    $null
}

function Line-Ms([string]$Line) { [double]::Parse(($Line.Trim() -split '\s+')[0], $Inv) }

function Find-Hidden($Run) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt 5000) {
        $h = [W]::FindWindow($HiddenClass, [NullString]::Value)
        if ($h -ne [IntPtr]::Zero) { return $h }
        Start-Sleep -Milliseconds 20
    }
    throw 'hidden window not found'
}

function Send-Cmd([IntPtr]$Hidden, [int]$Cmd) {
    [W]::PostMessage($Hidden, [uint32]($WM_APP + $Cmd), [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
}

function Stats([double[]]$Values) {
    $s = $Values | Sort-Object
    $median = $s[[int][math]::Floor(($s.Count - 1) / 2)]
    "min {0}  median {1}  max {2} ms (n={3})" -f (Num $s[0]), (Num $median), (Num $s[-1]), $s.Count
}

function Median([double[]]$Values) {
    $s = $Values | Sort-Object
    $s[[int][math]::Floor(($s.Count - 1) / 2)]
}

function Do-Size {
    Say '--- size'
    $bytes = (Get-Item $Exe).Length
    Check 'exe size < 15 MB (NFR-PERF-03)' ($bytes -lt 15MB) ("{0} MiB, stripped, LTO, static CRT" -f (Num ($bytes / 1MB) 'F2'))
}

function Measure-Start([string]$Label, [string]$Pattern, [string[]]$SpikeArgs, [int]$Runs, [switch]$Cold) {
    $times = @()
    for ($i = 1; $i -le $Runs; $i++) {
        Stop-Spike
        $path = $Exe
        if ($Cold) {
            # An unbuffered copy leaves the new file's pages out of the file cache. System DLLs stay
            # cached, as they would be on any logged-in desktop.
            $dir = Join-Path $RunDir "cold-$i"
            Remove-Item $dir -Recurse -ErrorAction SilentlyContinue
            & robocopy (Split-Path $Exe) $dir (Split-Path $Exe -Leaf) /J /NJH /NJS /NFL /NDL /NP | Out-Null
            $path = Join-Path $dir (Split-Path $Exe -Leaf)
        }
        $run = Start-Spike "start.log" $SpikeArgs $path
        $line = Wait-Line $run $Pattern 10000
        if ($line) { $times += (Line-Ms $line) }
        Start-Sleep -Milliseconds 100
        Send-Cmd (Find-Hidden $run) $CMD_QUIT
        $run.Proc.WaitForExit(3000) | Out-Null
    }
    Stop-Spike
    if ($Cold) { Remove-Item (Join-Path $RunDir 'cold-*') -Recurse -Force -ErrorAction SilentlyContinue }
    Say ("{0}: {1}" -f $Label, (Stats $times))
    Median $times
}

function Do-Start {
    Say '--- start-up (process creation to log line)'
    $warm = Measure-Start 'warm, first frame' ' first frame$' @() 15
    Check 'warm start to first frame < 250 ms (NFR-PERF-01)' ($warm -lt 250) ((Num $warm) + ' ms median')
    $cold = Measure-Start 'cold exe, first frame' ' first frame$' @() 8 -Cold
    Check 'cold-exe start to first frame < 250 ms (NFR-PERF-01)' ($cold -lt 250) ((Num $cold) + ' ms median')
    $tray = Measure-Start '--tray, tray added' ' tray added' @('--tray') 15
    Check '--tray start to tray icon < 150 ms (NFR-PERF-02)' ($tray -lt 150) ((Num $tray) + ' ms median')
}

function Do-Behaviour {
    Say '--- behaviour'
    Stop-Spike
    $run = Start-Spike 'behaviour.log'
    Check 'first frame logged' ([bool](Wait-Line $run ' first frame$')) ''
    $pixels = Wait-Line $run 'redraw 1 done colours' 2000
    Check 'first frame really on screen (pixels in the window)' ($pixels -match 'colours=Some\(([2-9]|\d\d+)\)') "$pixels"
    # Not a check: Shell_NotifyIconGetRect fails on some runs although the icon is there, and tray-icon
    # ignores NIM_ADD's result. The picture is for a person (or the AI) to look at; H-05 confirms it.
    Say ('  ' + (Wait-Line $run 'tray after 1 s' 3000))
    Add-Type -AssemblyName System.Windows.Forms
    $screen = [Windows.Forms.Screen]::PrimaryScreen.Bounds
    [W]::ShotCorner($screen.Right, $screen.Bottom, (Join-Path $RunDir 'tray.png'))
    $hidden = Find-Hidden $run
    Check 'hidden window exists and is not visible' (-not [W]::IsWindowVisible($hidden)) ''

    $window = [W]::FindWindow([NullString]::Value, $Title)
    Check 'main window visible at start' ([W]::IsWindowVisible($window)) ''
    Start-Sleep -Milliseconds 500
    $colours = [W]::Shot($window, (Join-Path $Results 'window.png'))
    Check 'window rendered (screenshot not blank)' ($colours -gt 8) ("$colours colours, results\window.png, " + [W]::GetDpiForWindow($window) + ' dpi')

    # Close button: the same WM_CLOSE the title bar's X sends.
    [W]::PostMessage($window, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
    Check 'close: request logged' ([bool](Wait-Line $run 'close requested')) ''
    Start-Sleep -Milliseconds 300
    Check 'close: window hidden' (-not [W]::IsWindowVisible($window)) ''
    Check 'close: process and event loop alive' (-not $run.Proc.HasExited) ''

    $n = (Read-Log $run).Count
    Send-Cmd $hidden $CMD_SHOW
    Check 'show after close: command reached the UI thread' ([bool](Wait-Line $run 'window shown' 3000 $n)) ''
    Start-Sleep -Milliseconds 300
    $again = [W]::FindWindow([NullString]::Value, $Title)
    Check 'show after close: window visible, same HWND' (([W]::IsWindowVisible($again)) -and ($again -eq $window)) ''
    $colours = [W]::Shot($again, (Join-Path $Results 'window-reshown.png'))
    Check 'show after close: rendered again' ($colours -gt 8) "$colours colours"

    Send-Cmd $hidden $CMD_TOGGLE
    Start-Sleep -Milliseconds 300
    Check 'toggle hides a visible window' (-not [W]::IsWindowVisible($window)) ''
    Send-Cmd $hidden $CMD_TOGGLE
    Start-Sleep -Milliseconds 300
    Check 'toggle shows a hidden window' ([W]::IsWindowVisible($window)) ''

    # Sent, not real: the plumbing only. Real sleep/resume is an H-05 item.
    $result = [IntPtr]::Zero
    foreach ($case in @(@(0x0004, 'PBT_APMSUSPEND'), @(0x0012, 'PBT_APMRESUMEAUTOMATIC'), @(0x0007, 'PBT_APMRESUMESUSPEND'))) {
        [W]::SendMessageTimeout($hidden, 0x0218, [IntPtr]$case[0], [IntPtr]::Zero, 2, 2000, [ref]$result) | Out-Null
        Check ("synthetic WM_POWERBROADCAST " + $case[1]) ([bool](Wait-Line $run ("WM_POWERBROADCAST " + $case[1]) 2000)) ''
    }
    [W]::SendMessageTimeout($hidden, 0x0011, [IntPtr]::Zero, [IntPtr][long]2147483648, 2, 2000, [ref]$result) | Out-Null
    Check 'synthetic WM_QUERYENDSESSION logged, returns TRUE' (([bool](Wait-Line $run 'WM_QUERYENDSESSION ENDSESSION_LOGOFF' 2000)) -and ($result -eq [IntPtr]1)) ''
    # wParam 0: "the session is not ending after all".
    [W]::SendMessageTimeout($hidden, 0x0016, [IntPtr]::Zero, [IntPtr][long]2147483648, 2, 2000, [ref]$result) | Out-Null
    Check 'synthetic WM_ENDSESSION logged' ([bool](Wait-Line $run 'WM_ENDSESSION ending=false' 2000)) ''

    Send-Cmd $hidden $CMD_QUIT
    Check 'quit: process exits' ($run.Proc.WaitForExit(5000)) ''
    Check 'quit: exit logged, in order' ([bool](Wait-Line $run ' exit$' 1000)) ''
    Copy-Item $run.Log (Join-Path $Results 'behaviour.log') -Force
}

function Sample-Process([int]$ProcId) {
    $p = Get-Process -Id $ProcId
    [pscustomobject]@{ Ws = $p.WorkingSet64 / 1MB; Priv = $p.PrivateMemorySize64 / 1MB; Cpu = $p.TotalProcessorTime.TotalSeconds; Handles = $p.HandleCount; Threads = $p.Threads.Count }
}

function Do-Memory {
    Say '--- memory and CPU (idle: no input, nothing mounted)'
    Stop-Spike
    $run = Start-Spike 'memory.log'
    Wait-Line $run ' first frame$' | Out-Null
    $hidden = Find-Hidden $run
    $a = Sample-Process $run.Proc.Id
    Start-Sleep -Seconds 60
    $b = Sample-Process $run.Proc.Id
    Say ("window visible, 60 s: WS {0} MiB, private {1} MiB, threads {2}, handles {3}, CPU {4} s in 60 s" -f (Num $b.Ws), (Num $b.Priv), $b.Threads, $b.Handles, (Num ($b.Cpu - $a.Cpu) 'F3'))
    Check 'working set after 60 s, visible < 40 MB (NFR-PERF-04)' ($b.Ws -lt 40) ((Num $b.Ws) + ' MiB')
    Check 'idle CPU, visible < 0.1% of one core' ((($b.Cpu - $a.Cpu) / 60) -lt 0.001) ((Num (($b.Cpu - $a.Cpu) / 60 * 100) 'F3') + '%')

    Send-Cmd $hidden $CMD_HIDE
    Start-Sleep -Seconds 2
    $c = Sample-Process $run.Proc.Id
    Start-Sleep -Seconds 60
    $d = Sample-Process $run.Proc.Id
    Say ("window hidden, 60 s: WS {0} MiB, private {1} MiB, threads {2}, handles {3}, CPU {4} s in 60 s" -f (Num $d.Ws), (Num $d.Priv), $d.Threads, $d.Handles, (Num ($d.Cpu - $c.Cpu) 'F3'))
    Check 'working set after 60 s, hidden < 40 MB' ($d.Ws -lt 40) ((Num $d.Ws) + ' MiB')
    Check 'idle CPU, hidden < 0.1% of one core' ((($d.Cpu - $c.Cpu) / 60) -lt 0.001) ((Num (($d.Cpu - $c.Cpu) / 60 * 100) 'F3') + '%')

    Send-Cmd $hidden $CMD_MEMORY
    Start-Sleep -Milliseconds 300
    Send-Cmd $hidden $CMD_QUIT
    $run.Proc.WaitForExit(5000) | Out-Null
    Copy-Item $run.Log (Join-Path $Results 'memory.log') -Force
}

# Idle CPU with the window visible, in separate 60 s windows, after 10 s to let start-up finish.
function Do-Idle {
    Say '--- idle CPU, window visible: 3 x 60 s after a 10 s settle'
    Stop-Spike
    $run = Start-Spike 'idle.log'
    Wait-Line $run ' first frame$' | Out-Null
    $hidden = Find-Hidden $run
    $s0 = Sample-Process $run.Proc.Id
    Start-Sleep -Seconds 10
    $prev = Sample-Process $run.Proc.Id
    Say ("  first 10 s after first frame: CPU {0} s" -f (Num ($prev.Cpu - $s0.Cpu) 'F3'))
    for ($i = 1; $i -le 3; $i++) {
        Start-Sleep -Seconds 60
        $now = Sample-Process $run.Proc.Id
        $pct = ($now.Cpu - $prev.Cpu) / 60 * 100
        Check "idle CPU, visible, window $i, < 0.1% of one core" ($pct -lt 0.1) ((Num $pct 'F3') + '%')
        $prev = $now
    }
    Send-Cmd $hidden $CMD_QUIT
    $run.Proc.WaitForExit(5000) | Out-Null
    Copy-Item $run.Log (Join-Path $Results 'idle.log') -Force
}

function Remove-ToastSetup {
    if (Test-Path $Exe) { Start-Process -Wait -FilePath $Exe -ArgumentList 'shortcut-remove' }
    foreach ($root in 'HKCU:\Software\Classes\AppUserModelId', 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Notifications\Settings') {
        if (Test-Path $root) {
            Get-ChildItem $root | Where-Object { $_.PSChildName -like "$BaseAumid*" } | Remove-Item -Recurse -Force
        }
    }
}

function Set-RegistryAumid([string]$Aumid) {
    $key = "HKCU:\Software\Classes\AppUserModelId\$Aumid"
    New-Item $key -Force | Out-Null
    New-ItemProperty $key -Name DisplayName -Value 'FS Manager spike' -PropertyType ExpandString -Force | Out-Null
}

function Install-Shortcut([string]$Aumid) {
    # A GUI exe does not block PowerShell, so wait for it and read its exit code.
    $p = Start-Process -FilePath $Exe -ArgumentList @('shortcut-install', $Aumid) -Wait -PassThru
    $programs = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
    $item = (New-Object -ComObject Shell.Application).NameSpace($programs).ParseName('FS Manager M0-3 Spike.lnk')
    $found = if ($item) { $item.ExtendedProperty('System.AppUserModel.ID') } else { $null }
    Check 'shortcut written with the AUMID (read back)' (($p.ExitCode -eq 0) -and ($found -eq $Aumid)) "exit $($p.ExitCode), read back '$found'"
}

function Try-Toast([string]$Variant, [string]$Aumid) {
    $run = Start-Spike "toast-$Variant.log" @('--tray', '--aumid', $Aumid)
    Wait-Line $run 'entering event loop' | Out-Null
    $hidden = Find-Hidden $run
    Start-Sleep -Milliseconds 500
    Send-Cmd $hidden $CMD_TOAST
    $shown = Wait-Line $run 'toast 1 (shown|error)' 3000
    Start-Sleep -Seconds 2
    Add-Type -AssemblyName System.Windows.Forms
    $area = [Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    [W]::ShotCorner($area.Right, $area.Bottom, (Join-Path $RunDir "toast-$Variant.png"))
    Send-Cmd $hidden $CMD_HISTORY
    $history = Wait-Line $run 'toast history' 2000
    # A toast no one touches times out after about 7 s.
    $end = Wait-Line $run 'toast 1 (dismissed|failed)' 20000
    Send-Cmd $hidden $CMD_QUIT
    $run.Proc.WaitForExit(5000) | Out-Null
    Copy-Item $run.Log (Join-Path $Results "toast-$Variant.log") -Force
    Start-Process -Wait -FilePath $Exe -ArgumentList @('toast-clear', $Aumid)
    Say ("{0}:" -f $Variant)
    Say ("  {0}" -f (((Read-Log $run) | Where-Object { $_ -match 'setting' }) -join ' '))
    Say ("  {0}" -f $shown)
    Say ("  {0}" -f $history)
    Say ("  {0}" -f $end)
    $end
}

function Do-Toast {
    Say '--- toasts: Windows accepting a toast does not prove it reached the screen; see H-05'
    Stop-Spike
    Remove-ToastSetup

    Try-Toast 'no-shortcut' "$BaseAumid.None" | Out-Null

    Set-RegistryAumid "$BaseAumid.Registry"
    Try-Toast 'registry' "$BaseAumid.Registry" | Out-Null
    Copy-Item (Join-Path $RunDir 'toast-registry.png') (Join-Path $Results 'toast-registry.png') -Force

    Install-Shortcut "$BaseAumid.Shortcut"
    Start-Sleep -Seconds 15  # the shell indexes a new Start-menu entry in its own time
    Try-Toast 'shortcut' "$BaseAumid.Shortcut" | Out-Null

    # What an installed copy would have: the shortcut and the registry key.
    Install-Shortcut "$BaseAumid.Both"
    Set-RegistryAumid "$BaseAumid.Both"
    Start-Sleep -Seconds 15
    Try-Toast 'both' "$BaseAumid.Both" | Out-Null

    Remove-ToastSetup
}

# How long after a shortcut is created does its toast appear? Pictures go to the run directory.
function Do-ToastWait {
    Say '--- toast with a shortcut, by time since the shortcut was created'
    Stop-Spike
    Remove-ToastSetup
    $aumid = "$BaseAumid.Wait"
    Install-Shortcut $aumid
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Add-Type -AssemblyName System.Windows.Forms
    $area = [Windows.Forms.Screen]::PrimaryScreen.WorkingArea
    $run = Start-Spike 'toast-wait.log' @('--tray', '--aumid', $aumid)
    Wait-Line $run 'entering event loop' | Out-Null
    $hidden = Find-Hidden $run
    foreach ($at in 20, 45, 90, 180, 300) {
        while ($sw.Elapsed.TotalSeconds -lt $at) { Start-Sleep -Milliseconds 200 }
        Send-Cmd $hidden $CMD_TOAST
        Start-Sleep -Seconds 2
        [W]::ShotCorner($area.Right, $area.Bottom, (Join-Path $RunDir "toast-wait-$at.png"))
        Say ("  toast sent at {0} s" -f $at)
    }
    Start-Sleep -Seconds 12
    Send-Cmd $hidden $CMD_QUIT
    $run.Proc.WaitForExit(5000) | Out-Null
    Copy-Item $run.Log (Join-Path $Results 'toast-wait.log') -Force
    Start-Process -Wait -FilePath $Exe -ArgumentList @('toast-clear', $aumid)
    Remove-ToastSetup
}

function Do-H05Start {
    Stop-Spike
    Remove-ToastSetup
    Set-RegistryAumid $BaseAumid
    $log = Join-Path $Results 'h05.log'
    Remove-Item $log -ErrorAction SilentlyContinue
    Start-Process -FilePath $Exe -ArgumentList @('--log', "`"$log`"")
    Write-Output "Spike running. Log: $log"
}

switch ($Action) {
    'size' { Do-Size }
    'start' { Do-Start }
    'behaviour' { Do-Behaviour }
    'memory' { Do-Memory }
    'idle' { Do-Idle }
    'toast' { Do-Toast }
    'toast-wait' { Do-ToastWait }
    'h05-start' { Do-H05Start }
    'h05-log' { Get-Content (Join-Path $Results 'h05.log') }
    'cleanup' { Stop-Spike; Remove-ToastSetup; Write-Output 'Cleaned up.' }
    'all' { Do-Size; Do-Behaviour; Do-Start; Do-Memory; Do-Toast }
    default { throw "Unknown action: $Action" }
}
if ($Passed + $Failed -gt 0) { Say ("=== {0} passed, {1} failed" -f $Passed, $Failed) }
