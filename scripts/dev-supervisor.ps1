$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$bootstrap = Join-Path $PSScriptRoot 'dev-up.ps1'
$shutdownPath = Join-Path $root '.dev\shutdown.request'
$restartDelaySeconds = 2
$maximumDelaySeconds = 30

Set-Location $root
while (-not (Test-Path -LiteralPath $shutdownPath)) {
    $startedAt = [DateTime]::UtcNow
    try {
        & $bootstrap -NoBrowser -ManagedBySupervisor
    }
    catch {
        Write-Warning "OmniEngine startup or runtime stopped: $($_.Exception.Message)"
    }

    if (Test-Path -LiteralPath $shutdownPath) {
        break
    }

    $uptime = [DateTime]::UtcNow - $startedAt
    if ($uptime.TotalSeconds -ge 60) {
        $restartDelaySeconds = 2
    }
    $delay = $restartDelaySeconds
    $restartDelaySeconds = [Math]::Min($restartDelaySeconds * 2, $maximumDelaySeconds)
    Write-Warning "Restarting OmniEngine gateway and dashboard in $delay seconds."
    for ($second = 0; $second -lt $delay; $second++) {
        if (Test-Path -LiteralPath $shutdownPath) {
            break
        }
        Start-Sleep -Seconds 1
    }
}

Remove-Item -LiteralPath $shutdownPath -Force -ErrorAction SilentlyContinue
Write-Output 'OmniEngine background supervisor stopped.'
