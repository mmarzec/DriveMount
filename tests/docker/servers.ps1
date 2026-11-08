# Manages the test servers, which run on Docker Engine inside WSL2 (no Docker Desktop).
#   servers.ps1 up      generate keys if missing, build and start the containers
#   servers.ps1 down    stop and remove the containers
#   servers.ps1 check   verify every auth method; key and certificate logins run from Windows
param([ValidateSet('up', 'down', 'check')][string]$Action = 'up')
$ErrorActionPreference = 'Stop'

$distro = 'Ubuntu-22.04'
$testsDir = Split-Path $PSScriptRoot -Parent

function Invoke-Wsl {
    & wsl.exe -d $distro -u root --cd $testsDir -- @args
    if ($LASTEXITCODE -ne 0) { throw "wsl $($args -join ' ') failed with exit code $LASTEXITCODE" }
}

function Test-SftpLogin([string]$Name, [string]$User, [string]$Key) {
    $tmp = Join-Path $env:TEMP 'drivemount-test-keys'
    # A batch file, not a pipe: PowerShell 5.1 prefixes piped text with a BOM that sftp rejects.
    $batch = Join-Path $tmp 'batch.txt'
    [IO.File]::WriteAllText($batch, "pwd`n")
    $opts = @('-P', '2222', '-b', $batch, '-o', 'BatchMode=yes', '-o', 'IdentitiesOnly=yes', '-o', 'LogLevel=ERROR',
        '-o', 'StrictHostKeyChecking=accept-new', '-o', "UserKnownHostsFile=$tmp\known_hosts",
        '-i', (Join-Path $tmp $Key))
    $null = & sftp.exe @opts "$User@localhost"
    $ok = $LASTEXITCODE -eq 0
    Write-Host ($(if ($ok) { 'ok  ' } else { 'FAIL' }) + " $Name")
    return $ok
}

switch ($Action) {
    'up' {
        Invoke-Wsl bash docker/gen-keys.sh
        Invoke-Wsl docker compose up -d --build
    }
    'down' {
        Invoke-Wsl docker compose down
    }
    'check' {
        # Windows OpenSSH refuses private keys readable by other users, so use private copies.
        $tmp = Join-Path $env:TEMP 'drivemount-test-keys'
        New-Item -ItemType Directory -Force $tmp | Out-Null
        foreach ($f in 'id_ed25519', 'id_cert', 'id_cert-cert.pub') {
            Remove-Item (Join-Path $tmp $f) -Force -ErrorAction SilentlyContinue
            Copy-Item (Join-Path $PSScriptRoot "keys\$f") $tmp
        }
        foreach ($f in 'id_ed25519', 'id_cert') {
            icacls (Join-Path $tmp $f) /inheritance:r /grant:r "$($env:USERNAME):F" | Out-Null
        }
        $results = @(
            Test-SftpLogin 'public key from Windows (tester)' tester id_ed25519
            Test-SftpLogin 'public key from Windows (keyonly)' keyonly id_ed25519
            Test-SftpLogin 'certificate from Windows (certuser)' certuser id_cert
        )
        $ok = $results -notcontains $false
        & wsl.exe -d $distro -u root --cd $testsDir -- docker compose exec -T sshd check-auth.sh
        if (-not $ok -or $LASTEXITCODE -ne 0) { throw 'Auth check failed' }
    }
}
