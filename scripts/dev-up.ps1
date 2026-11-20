param(
    [switch]$NoBrowser,
    [switch]$ManagedBySupervisor
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$devDir = Join-Path $root '.dev'
$shutdownPath = Join-Path $devDir 'shutdown.request'
$uiProcess = $null

function Assert-PortAvailable {
    param(
        [int]$Port,
        [string]$Service
    )

    $listener = [System.Net.Sockets.TcpListener]::new(
        [System.Net.IPAddress]::Loopback,
        $Port
    )
    $started = $false
    try {
        $listener.Start()
        $started = $true
    }
    catch {
        throw "Cannot start ${Service}: 127.0.0.1:$Port is already in use or unavailable. Find the owner with: Get-NetTCPConnection -LocalPort $Port -State Listen | Select-Object OwningProcess"
    }
    finally {
        if ($started) {
            $listener.Stop()
        }
    }
}

function Get-LogTail {
    param([string]$Path)

    if (Test-Path -LiteralPath $Path) {
        return (Get-Content -LiteralPath $Path -Tail 30 | Out-String)
    }
    return '(no startup log was created)'
}

function Protect-LocalSecret {
    param([string]$Path)

    $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
    if ($null -eq $identity.User) {
        throw 'Cannot determine the current Windows user to protect local development secrets.'
    }
    $grant = '*{0}:(F)' -f $identity.User.Value
    & icacls.exe $Path /inheritance:r /grant:r $grant | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Could not restrict $Path to the current Windows user. Check the file ACLs and retry."
    }
}

function New-LocalApiKey {
    $bytes = New-Object 'System.Byte[]' 32
    $generator = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $generator.GetBytes($bytes)
    }
    finally {
        $generator.Dispose()
    }
    return [System.BitConverter]::ToString($bytes).Replace('-', '').ToLowerInvariant()
}

function Stop-StartedProcess {
    param($Process)

    if ($null -ne $Process -and -not $Process.HasExited) {
        Stop-Process -Id $Process.Id
        Wait-Process -Id $Process.Id -ErrorAction SilentlyContinue
    }
}

function Stop-ManagedGateway {
    $pidPath = Join-Path $devDir 'gateway.pid'
    if (-not (Test-Path -LiteralPath $pidPath)) {
        return
    }
    $rawPid = [System.IO.File]::ReadAllText($pidPath).Trim()
    $gatewayBinary = Join-Path $root 'target\debug\omni-gateway.exe'
    $processId = 0
    if ([int]::TryParse($rawPid, [ref]$processId) -and $processId -gt 0) {
        $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
        if ($null -ne $process -and $process.Path -and
            [string]::Equals(
                [System.IO.Path]::GetFullPath($process.Path),
                [System.IO.Path]::GetFullPath($gatewayBinary),
                [System.StringComparison]::OrdinalIgnoreCase
            )) {
            Stop-Process -Id $processId -ErrorAction SilentlyContinue
            Wait-Process -Id $processId -ErrorAction SilentlyContinue
        }
    }
    Remove-Item -LiteralPath $pidPath -Force -ErrorAction SilentlyContinue
}

try {
    Set-Location $root
    $cargo = Get-Command cargo -CommandType Application -ErrorAction SilentlyContinue
    if ($null -eq $cargo) {
        throw 'Rust/Cargo was not found on PATH. Install the Rust toolchain, reopen PowerShell, and retry.'
    }
    $node = Get-Command node -CommandType Application -ErrorAction SilentlyContinue
    if ($null -eq $node) {
        throw 'Node.js was not found on PATH. Install Node.js, reopen PowerShell, and retry.'
    }

    Assert-PortAvailable -Port 8443 -Service 'omni-gateway'
    Assert-PortAvailable -Port 3000 -Service 'the dashboard'

    New-Item -ItemType Directory -Path $devDir -Force | Out-Null
    if (-not $ManagedBySupervisor) {
        Remove-Item -LiteralPath $shutdownPath -Force -ErrorAction SilentlyContinue
    }
    $templatePath = Join-Path $root 'config\cloud-api.example.toml'
    $configPath = Join-Path $devDir 'gateway.toml'
    $certificatePath = Join-Path $devDir 'cert.pem'
    $privateKeyPath = Join-Path $devDir 'key.pem'
    $apiKeyPath = Join-Path $devDir 'api-key.txt'
    $adminTokenPath = Join-Path $devDir 'key-admin.txt'
    $statePath = Join-Path $devDir 'gateway-state.json'

    $gitignorePath = Join-Path $root '.gitignore'
    $gitignore = [System.IO.File]::ReadAllText($gitignorePath)
    if (-not [regex]::IsMatch($gitignore, '(?m)^\.dev/?\s*$')) {
        throw '.dev is not gitignored. Add .dev/ to the repository .gitignore before starting local services.'
    }

    $apiKeys = ''
    if (Test-Path -LiteralPath $apiKeyPath) {
        $apiKeys = [System.IO.File]::ReadAllText($apiKeyPath).Trim()
    }
    if ([string]::IsNullOrWhiteSpace($apiKeys)) {
        $apiKeys = 'local:' + (New-LocalApiKey)
        [System.IO.File]::WriteAllText($apiKeyPath, $apiKeys, [System.Text.Encoding]::ASCII)
    } else {
        $entries = $apiKeys -split ','
        for ($index = 0; $index -lt $entries.Length; $index++) {
            $parts = $entries[$index].Trim() -split ':', 2
            if ($parts.Length -ne 2 -or
                [string]::IsNullOrWhiteSpace($parts[0]) -or
                $parts[1].Trim().Length -lt 24 -or
                $parts[1].Trim().Length -gt 512) {
                throw ('The local API key file contains an invalid entry at position ' +
                    ($index + 1) +
                    '. Expected comma-separated tenant:key entries with keys between 24 and 512 characters.')
            }
        }
    }
    Protect-LocalSecret -Path $apiKeyPath

    $keyAdminToken = ''
    if (Test-Path -LiteralPath $adminTokenPath) {
        $keyAdminToken = [System.IO.File]::ReadAllText($adminTokenPath).Trim()
    }
    if ([string]::IsNullOrWhiteSpace($keyAdminToken)) {
        $keyAdminToken = 'omni_admin_' + (New-LocalApiKey)
        [System.IO.File]::WriteAllText($adminTokenPath, $keyAdminToken, [System.Text.Encoding]::ASCII)
    }
    if ($keyAdminToken -notmatch '^[\x21-\x7E]{32,512}$') {
        throw 'The local key-administration token is invalid; expected 32-512 visible ASCII characters.'
    }
    Protect-LocalSecret -Path $adminTokenPath

    $configText = [System.IO.File]::ReadAllText($templatePath)
    $certificateTomlPath = $certificatePath.Replace('\', '/')
    $privateKeyTomlPath = $privateKeyPath.Replace('\', '/')
    $replacements = @(
        [pscustomobject]@{ Pattern = '(?m)^bind\s*=.*$'; Replacement = 'bind = "127.0.0.1:8443"' },
        [pscustomobject]@{ Pattern = '(?m)^tls_cert\s*=.*$'; Replacement = 'tls_cert = "' + $certificateTomlPath + '"' },
        [pscustomobject]@{ Pattern = '(?m)^tls_key\s*=.*$'; Replacement = 'tls_key = "' + $privateKeyTomlPath + '"' }
    )
    foreach ($replacement in $replacements) {
        if (-not [regex]::IsMatch($configText, $replacement.Pattern)) {
            throw "Cloud API config template is missing expected setting '$($replacement.Pattern)'."
        }
        $configText = [regex]::Replace($configText, $replacement.Pattern, $replacement.Replacement)
    }
    if (-not [regex]::IsMatch($configText, '(?m)^token_secret_source\s*=\s*"env:OMNI_API_KEYS"\s*$')) {
        throw 'Cloud API config must use token_secret_source = "env:OMNI_API_KEYS" for the local bootstrap.'
    }
    [System.IO.File]::WriteAllText(
        $configPath,
        $configText,
        [System.Text.UTF8Encoding]::new($false)
    )

    Write-Host 'Building the gateway and Rust-native local certificate helper...'
    & $cargo.Source build -p omni-gateway --bin omni-gateway --bin omni-gateway-dev-cert
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo build failed with exit code $LASTEXITCODE."
    }

    $targetDir = Join-Path $root 'target\debug'
    $certificateHelper = Join-Path $targetDir 'omni-gateway-dev-cert.exe'
    $gatewayBinary = Join-Path $targetDir 'omni-gateway.exe'
    if (-not (Test-Path -LiteralPath $certificateHelper) -or -not (Test-Path -LiteralPath $gatewayBinary)) {
        throw 'Cargo completed without producing the expected gateway binaries. Check Cargo target configuration.'
    }
    & $certificateHelper $certificatePath $privateKeyPath
    if ($LASTEXITCODE -ne 0) {
        throw "Local TLS certificate generation failed with exit code $LASTEXITCODE."
    }
    Protect-LocalSecret -Path $privateKeyPath

    $env:OMNI_USAGE_POLICY = Join-Path $root 'policies\usage-tiers.toml'
    $env:OMNI_GATEWAY_CONFIG = $configPath
    $env:OMNI_GATEWAY_STATE = $statePath
    $env:OMNI_API_KEYS = $apiKeys
    $env:OMNI_GATEWAY_URL = 'https://127.0.0.1:8443'
    $env:OMNI_GATEWAY_INSECURE_TLS = '1'
    $localApiKeyParts = (($apiKeys -split ',')[0] -split ':', 2)
    $env:OMNI_DEV_API_KEY = $localApiKeyParts[1].Trim()
    $env:OMNI_KEY_ADMIN_TOKEN = $keyAdminToken
    $env:OMNI_DEV_KEY_ADMIN_TOKEN = $keyAdminToken
    $env:OMNI_KEY_ADMIN_ENABLED = '1'
    $env:OMNI_DEV_MANAGED = '1'
    $env:OMNI_DEV_KEY_FILE = $apiKeyPath
    $env:OMNI_GATEWAY_BINARY = Join-Path $root 'target\debug\omni-gateway.exe'
    $env:OMNI_DEV_USER_SID = [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $env:PORT = '3000'

    $uiStdout = Join-Path $devDir 'ui.stdout.log'
    $uiStderr = Join-Path $devDir 'ui.stderr.log'
    $uiScript = Join-Path $root 'ui\server.js'
    $dashboardUrl = 'http://127.0.0.1:3000'

    $uiProcess = Start-Process `
        -FilePath $node.Source `
        -ArgumentList ('"' + $uiScript + '"') `
        -WorkingDirectory $root `
        -RedirectStandardOutput $uiStdout `
        -RedirectStandardError $uiStderr `
        -PassThru `
        -WindowStyle Hidden

    $ready = $false
    $deadline = [DateTime]::UtcNow.AddSeconds(45)
    while ([DateTime]::UtcNow -lt $deadline) {
        if ($uiProcess.HasExited) {
            throw "The dashboard server exited during startup.`n$(Get-LogTail $uiStderr)$(Get-LogTail $uiStdout)"
        }
        try {
            $healthResponse = Invoke-WebRequest `
                -UseBasicParsing `
                -Uri "$dashboardUrl/api/health" `
                -TimeoutSec 2
            $health = $healthResponse.Content | ConvertFrom-Json
            if ($healthResponse.StatusCode -eq 200 -and $health.status -eq 'ok') {
                $ready = $true
                break
            }
        }
        catch {
        }
        Start-Sleep -Milliseconds 500
    }
    if (-not $ready) {
        throw "Gateway health did not become ready within 45 seconds. Gateway log:`n$(Get-LogTail (Join-Path $devDir 'gateway.stderr.log'))$(Get-LogTail (Join-Path $devDir 'gateway.stdout.log'))"
    }

    Write-Host "Gateway online: https://127.0.0.1:8443"
    Write-Host "Dashboard: $dashboardUrl"
    Write-Host 'The local API key is loaded automatically in this browser session.'
    if (-not $NoBrowser) {
        try {
            Start-Process -FilePath $dashboardUrl
        }
        catch {
            Write-Warning "Could not open the default browser automatically. Browse to $dashboardUrl"
        }
    }
    Write-Host 'Press Ctrl+C in this PowerShell window to stop both services.'

    $shutdownRequested = $false
    while (-not $uiProcess.HasExited) {
        if (Test-Path -LiteralPath $shutdownPath) {
            if (-not $ManagedBySupervisor) {
                Remove-Item -LiteralPath $shutdownPath -Force
            }
            $shutdownRequested = $true
            break
        }
        try {
            $bridgeConfig = Invoke-RestMethod `
                -UseBasicParsing `
                -Uri "$dashboardUrl/api/config" `
                -TimeoutSec 2
            if ($bridgeConfig.managed_gateway_status -eq 'error') {
                throw "Managed gateway stopped: $($bridgeConfig.managed_gateway_error)"
            }
        }
        catch {
            if ($_.Exception.Message -like 'Managed gateway stopped:*') {
                throw
            }
        }
        Start-Sleep -Seconds 1
    }
    if ($shutdownRequested) {
        Write-Host 'Background development services stopped.'
        return
    }
    throw "The dashboard server stopped unexpectedly.`n$(Get-LogTail $uiStderr)$(Get-LogTail $uiStdout)"
}
finally {
    Stop-StartedProcess -Process $uiProcess
    Stop-ManagedGateway
}
