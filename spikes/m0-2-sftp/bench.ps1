# Spike M0-2 driver. Run `m0-2-sftp.exe prepare` once first; the test servers must be up.
#   bench.ps1 matrix   -Profile lan|wan [-Cap 20]   chunk x in-flight x operation (time-capped cells)
#   bench.ps1 final    -Profile lan|wan [-Reps 3]   whole 1 GiB runs of the candidates, and the sftp.exe baseline
#   bench.ps1 tune                                  window, SSH packet size and cipher variations
#   bench.ps1 ciphers                               repeated downloads by cipher and requests in flight
#   bench.ps1 probes   -Profile lan|wan             listing, stat, head-of-line blocking
#   bench.ps1 failures                              keep-alive and connection-drop detection
#   bench.ps1 net      -Profile lan|wan|show        switch the network profile only
#
# -Via vm (default) connects to the WSL VM's IP. -Via relay uses localhost:2222, through the Windows
# wslrelay port forward, which stalls single small writes (see the report, section 3).
param(
    [Parameter(Mandatory)][ValidateSet('matrix', 'final', 'tune', 'ciphers', 'probes', 'failures', 'net')][string]$Action,
    [string]$Profile = 'lan',
    [ValidateSet('vm', 'relay')][string]$Via = 'vm',
    [int]$Reps = 3,
    [int]$Cap = 20
)
$ErrorActionPreference = 'Stop'

$here = $PSScriptRoot
$exe = Join-Path $here 'target\release\m0-2-sftp.exe'
$results = Join-Path $here 'results'
$localFile = Join-Path $here 'target\m02-testdata.bin'
$invariant = [Globalization.CultureInfo]::InvariantCulture

function Invoke-Net {
    & wsl.exe -d Ubuntu-22.04 -u root --cd $here -- bash net.sh @args
    if ($LASTEXITCODE -ne 0) { throw "net.sh $args failed" }
}

$target = if ($Via -eq 'relay') { 'localhost' } else { (& wsl.exe -d Ubuntu-22.04 -- hostname -I).Trim().Split(' ')[0] }

function Invoke-Bench {
    & $exe @args --host $target
    if ($LASTEXITCODE -ne 0) { throw "m0-2-sftp $args failed" }
}

# Runs the benchmark and keeps its output, for commands that print a report instead of CSV rows.
function Write-Log([string]$File, [string[]]$Lines) {
    $Lines | Out-File -FilePath $File -Append -Encoding ascii
    $Lines | Write-Host
}

function Write-Report([string]$File, [string]$Title, [string[]]$BenchArgs) {
    Write-Log $File "--- $Title"
    $output = @(& $exe @BenchArgs --host $target)
    $code = $LASTEXITCODE
    if ($output) { Write-Log $File $output }
    if ($code -ne 0) { Write-Log $File "exit code $code" }
}

function Invoke-Sftp([string]$Command, [string[]]$Extra = @()) {
    # The key copy that tests\docker\servers.ps1 check makes with owner-only permissions.
    $keyDir = Join-Path $env:TEMP 'fsm-test-keys'
    # A batch file, not a pipe: PowerShell 5.1 prefixes piped text with a BOM that sftp rejects.
    $batch = Join-Path $keyDir 'm02-batch.txt'
    [IO.File]::WriteAllText($batch, "$Command`n")
    $opts = @('-P', '2222', '-b', $batch, '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'LogLevel=ERROR',
        '-o', 'StrictHostKeyChecking=accept-new', '-o', "UserKnownHostsFile=$keyDir\known_hosts",
        '-i', (Join-Path $keyDir 'id_ed25519')) + $Extra
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $null = & sftp.exe @opts "tester@$target"
    if ($LASTEXITCODE -ne 0) { throw "sftp.exe failed: $Command" }
    return $watch.Elapsed.TotalSeconds
}

function Write-Baseline([string]$Csv, [string]$Label, [string[]]$Extra = @()) {
    if (-not (Test-Path $Csv)) { 'profile,op,rep,secs,mib_s' | Out-File -Encoding ascii $Csv }
    $size = (Get-Item $localFile).Length
    # Neither side touches a local disk on download: get goes to NUL, and our benchmark discards after
    # comparing. For put, a warm read cache stands in for our in-memory upload.
    $null = Get-FileHash $localFile -Algorithm MD5
    $ops = @(
        @{ Name = 'get-nul'; Cmd = 'get m02/file.bin NUL' },
        @{ Name = 'put'; Cmd = "put `"$($localFile -replace '\\','/')`" m02/upload.bin" }
    )
    foreach ($op in $ops) {
        foreach ($rep in 1..$Reps) {
            $secs = Invoke-Sftp $op.Cmd $Extra
            $line = [string]::Format($invariant, '{0},{1},{2},{3:F2},{4:F1}', $Label, $op.Name, $rep, $secs, ($size / 1MB / $secs))
            Write-Host $line
            $line | Out-File -Encoding ascii -Append $Csv
        }
    }
}

switch ($Action) {
    'net' { Invoke-Net $Profile }
    'matrix' {
        Invoke-Net $Profile
        Invoke-Bench matrix --profile $Profile --cap $Cap --csv (Join-Path $results "matrix-$Profile-$Via.csv")
    }
    'final' {
        Invoke-Net $Profile
        Write-Baseline (Join-Path $results "final-baseline-$Profile.csv") $Profile
        $csv = Join-Path $results "final-$Profile.csv"
        $cells = @(
            @('download', '65536', '64'),
            @('download', '0', '32'),
            @('download', '0', '64'),
            @('download', '0', '16'),
            @('upload-free', '0', '16'), # About 4 MiB outstanding: the chosen write-behind bound.
            @('upload-free', '0', '32'), # About 8 MiB outstanding: the bound the spec first proposed.
            @('upload-free', '0', '64'),
            @('upload-percall', '0', '64')
        )
        foreach ($cell in $cells) {
            foreach ($rep in 1..$Reps) {
                Invoke-Bench run --profile $Profile --op $cell[0] --chunk $cell[1] --inflight $cell[2] --verify 1 --csv $csv
            }
        }
    }
    'tune' {
        $csv = Join-Path $results 'tune.csv'
        Invoke-Net wan
        foreach ($window in '2', '16', '64') {
            Invoke-Bench run --profile wan-window --op download --chunk 0 --inflight 64 --window-mib $window --cap 30 --csv $csv
        }
        Invoke-Bench run --profile wan-packet64k --op download --chunk 0 --inflight 64 --max-packet 65535 --cap 30 --csv $csv
        Invoke-Bench run --profile wan-chanbuf1000 --op download --chunk 0 --inflight 64 --channel-buffer 1000 --cap 30 --csv $csv
        Invoke-Net lan
        foreach ($cipher in 'chacha20', 'aes256gcm', 'aes128gcm') {
            foreach ($op in 'download', 'upload-free') {
                Invoke-Bench run --profile lan-cipher --op $op --chunk 0 --inflight 64 --cipher $cipher --csv $csv
            }
        }
        foreach ($sftpCipher in 'aes256-gcm@openssh.com', 'aes128-gcm@openssh.com') {
            Write-Baseline (Join-Path $results 'tune-baseline.csv') "lan-$sftpCipher" @('-c', $sftpCipher)
        }
    }
    'probes' {
        Invoke-Net $Profile
        $file = Join-Path $results "probes-$Profile.txt"
        Write-Report $file 'info' @('info')
        Write-Report $file 'meta' @('meta')
        foreach ($op in 'download', 'upload-free') {
            foreach ($inflight in '64', '32', '16') {
                Write-Report $file "hol $op, largest chunk, $inflight in flight" @('hol', '--op', $op, '--chunk', '0', '--inflight', $inflight)
            }
        }
        Invoke-Net lan
    }
    'failures' {
        $file = Join-Path $results 'failures.txt'
        Invoke-Net lan
        # Spec defaults: keepalive_secs = 15, op_timeout_secs = 15.
        $spec = @('--keepalive', '15', '--request-timeout', '15')
        Write-Report $file 'server closes abruptly, idle session' (@('drop', '--trigger', 'kill', '--limit', '30') + $spec)
        Write-Report $file 'server closes abruptly, 8 reads in flight' (@('drop', '--trigger', 'kill', '--busy', '1', '--limit', '30') + $spec)
        Write-Report $file 'silent link loss, idle session' (@('drop', '--trigger', 'blackhole', '--limit', '90') + $spec)
        Invoke-Net lan
        Write-Report $file 'silent link loss, 8 reads in flight' (@('drop', '--trigger', 'blackhole', '--busy', '1', '--limit', '90') + $spec)
        Invoke-Net lan

        Write-Report $file 'server hangs but TCP stays up (SIGSTOP), idle session' (@('drop', '--trigger', 'freeze', '--limit', '90') + $spec)
        Invoke-Net kill tester
        Write-Report $file 'server hangs but TCP stays up (SIGSTOP), 8 reads in flight' (@('drop', '--trigger', 'freeze', '--busy', '1', '--limit', '90') + $spec)
        Invoke-Net kill tester

        Invoke-Net alive on
        Write-Report $file 'server sends keepalive@openssh.com every 1 s; 2 misses end the session; client idle 15 s' @('keepalive', '--user', 'keyonly', '--idle', '15')
        # Control: proves those probes are live, because a client that cannot answer them is dropped.
        Write-Report $file 'control: replies from the server dropped, so its probes get no answer' @('drop', '--user', 'keyonly', '--trigger', 'blackhole', '--limit', '8', '--request-timeout', '3')
        Invoke-Net lan
        # bash, so the log's stderr is merged in WSL: PowerShell 5.1 would turn it into error records.
        $log = @(& wsl.exe -d Ubuntu-22.04 -u root -- bash -c 'docker logs --since 2m fsm-test-sshd-1 2>&1 | grep -e not.responding -e Timeout || true')
        Write-Log $file (@('--- sshd log, last 2 minutes') + $log)
        Invoke-Net alive off
    }
    'ciphers' {
        Invoke-Net lan
        $csv = Join-Path $results 'tune-ciphers.csv'
        foreach ($cipher in 'chacha20', 'aes256gcm') {
            foreach ($inflight in '16', '32', '64') {
                foreach ($rep in 1..5) {
                    Invoke-Bench run --profile lan --op download --chunk 0 --inflight $inflight --cipher $cipher --csv $csv
                }
            }
        }
    }
}
