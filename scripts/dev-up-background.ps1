$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$devDir = Join-Path $root '.dev'
$pidPath = Join-Path $devDir 'dev-up-background.pid'
$shutdownPath = Join-Path $devDir 'shutdown.request'
$stdoutPath = Join-Path $devDir 'bootstrap.stdout.log'
$stderrPath = Join-Path $devDir 'bootstrap.stderr.log'

New-Item -ItemType Directory -Path $devDir -Force | Out-Null
if (Test-Path -LiteralPath $pidPath) {
    $existingPid = 0
    $rawPid = [System.IO.File]::ReadAllText($pidPath).Trim()
    if ([int]::TryParse($rawPid, [ref]$existingPid) -and $existingPid -gt 0) {
        $existing = Get-CimInstance Win32_Process -Filter "ProcessId=$existingPid" -ErrorAction SilentlyContinue
        $isSupervisor = $null -ne $existing -and $existing.CommandLine -like '*scripts\dev-supervisor.ps1*'
        $isBootstrap = $null -ne $existing -and $existing.CommandLine -like '*scripts\dev-up.ps1*'
        if ($isSupervisor -or $isBootstrap) {
            throw "OmniEngine development services are already running (PID $existingPid). Use .\scripts\dev-down-background.ps1 to stop them."
        }
    }
    Remove-Item -LiteralPath $pidPath -Force
}

Remove-Item -LiteralPath $shutdownPath -Force -ErrorAction SilentlyContinue
$powershell = (Get-Process -Id $PID).Path
$supervisor = Join-Path $PSScriptRoot 'dev-supervisor.ps1'
$arguments = @(
    '-NoProfile',
    '-ExecutionPolicy', 'Bypass',
    '-File', "`"$supervisor`""
)
$process = Start-Process `
    -FilePath $powershell `
    -ArgumentList $arguments `
    -WorkingDirectory $root `
    -WindowStyle Hidden `
    -RedirectStandardOutput $stdoutPath `
    -RedirectStandardError $stderrPath `
    -PassThru
[System.IO.File]::WriteAllText($pidPath, [string]$process.Id, [System.Text.Encoding]::ASCII)

$deadline = [DateTime]::UtcNow.AddSeconds(180)
while ([DateTime]::UtcNow -lt $deadline) {
    $process.Refresh()
    if ($process.HasExited) {
        Remove-Item -LiteralPath $pidPath -Force -ErrorAction SilentlyContinue
        $stderr = if (Test-Path -LiteralPath $stderrPath) { Get-Content -LiteralPath $stderrPath -Tail 30 | Out-String } else { '' }
        $stdout = if (Test-Path -LiteralPath $stdoutPath) { Get-Content -LiteralPath $stdoutPath -Tail 30 | Out-String } else { '' }
        throw "OmniEngine background startup failed.`n$stderr$stdout"
    }
    try {
        $health = Invoke-RestMethod -Uri 'http://127.0.0.1:3000/api/health' -TimeoutSec 2
        if ($health.status -eq 'ok') {
            Write-Host "OmniEngine gateway and dashboard are running in the background (PID $($process.Id))."
            Write-Host 'Dashboard: http://127.0.0.1:3000'
            Write-Host 'Stop with: .\scripts\dev-down-background.ps1'
            exit 0
        }
    } catch {
    }
    Start-Sleep -Milliseconds 500
}

New-Item -ItemType File -Path $shutdownPath -Force | Out-Null
Wait-Process -Id $process.Id -Timeout 30 -ErrorAction SilentlyContinue
$process.Refresh()
if (-not $process.HasExited) {
    $remaining = Get-CimInstance Win32_Process -Filter "ProcessId=$($process.Id)" -ErrorAction SilentlyContinue
    if ($null -eq $remaining -or $remaining.CommandLine -notlike '*scripts\dev-supervisor.ps1*') {
        throw "Startup timed out and PID $($process.Id) no longer identifies the OmniEngine supervisor; inspect .dev\dev-up-background.pid before stopping it."
    }
    throw 'Startup timed out. The supervisor is still active; inspect .dev\bootstrap.stderr.log and stop it with .\scripts\dev-down-background.ps1 if needed.'
}
Remove-Item -LiteralPath $pidPath -Force -ErrorAction SilentlyContinue
throw 'OmniEngine background startup timed out. Inspect .dev\bootstrap.stderr.log and the gateway logs.'
