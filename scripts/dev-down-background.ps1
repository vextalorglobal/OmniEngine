$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$devDir = Join-Path $root '.dev'
$pidPath = Join-Path $devDir 'dev-up-background.pid'
$shutdownPath = Join-Path $devDir 'shutdown.request'

if (-not (Test-Path -LiteralPath $pidPath)) {
    Write-Host 'OmniEngine background services are not running.'
    exit 0
}

$processId = 0
$rawPid = [System.IO.File]::ReadAllText($pidPath).Trim()
if (-not [int]::TryParse($rawPid, [ref]$processId) -or $processId -le 0) {
    throw 'The OmniEngine background PID file is invalid. Inspect .dev\dev-up-background.pid before removing it.'
}
$process = Get-CimInstance Win32_Process -Filter "ProcessId=$processId" -ErrorAction SilentlyContinue
if ($null -eq $process) {
    Remove-Item -LiteralPath $pidPath -Force
    Remove-Item -LiteralPath $shutdownPath -Force -ErrorAction SilentlyContinue
    Write-Host 'Removed a stale OmniEngine background PID file.'
    exit 0
}
$isSupervisor = $process.CommandLine -like '*scripts\dev-supervisor.ps1*'
$isBootstrap = $process.CommandLine -like '*scripts\dev-up.ps1*'
if (-not $isSupervisor -and -not $isBootstrap) {
    throw "PID $processId does not identify the OmniEngine bootstrap; refusing to stop it."
}

New-Item -ItemType File -Path $shutdownPath -Force | Out-Null
Wait-Process -Id $processId -Timeout 30 -ErrorAction SilentlyContinue
$remaining = Get-Process -Id $processId -ErrorAction SilentlyContinue
if ($null -ne $remaining) {
    throw "OmniEngine did not stop gracefully. Inspect .dev\bootstrap.stderr.log and the gateway logs before stopping PID $processId."
}
Remove-Item -LiteralPath $pidPath -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $shutdownPath -Force -ErrorAction SilentlyContinue
Write-Host 'OmniEngine gateway and dashboard stopped.'
