#!/usr/bin/env pwsh
<#
.SYNOPSIS
  Build `liftgate` in release mode (plus embedded web UI if Bun is installed) and start the server.

.EXAMPLES
  # Build release and run gateway in the foreground (Ctrl+C to stop)
  .\run-gateway.cmd

  # Build release and start gateway in the background (.agy-runtime/gateway.pid)
  .\run-gateway.cmd -Daemon

  # Stop background gateway started with -Daemon
  .\run-gateway.cmd -Stop

  # Pass any liftgate flags directly (e.g. custom port, model, or API key)
  .\run-gateway.cmd --listen 127.0.0.1:8787 --default-model gemini-4-carbon-high --api-key local-agy
#>
$ErrorActionPreference = "Stop"

$RootDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RuntimeDir = Join-Path $RootDir ".agy-runtime"
New-Item -ItemType Directory -Force -Path $RuntimeDir | Out-Null

$PidFile = Join-Path $RuntimeDir "gateway.pid"
$LogOut  = Join-Path $RuntimeDir "gateway.out.log"
$LogErr  = Join-Path $RuntimeDir "gateway.err.log"

$Daemon = $false
$Stop = $false
$SkipBuild = $false
$RebuildWeb = $false
$HasListen = $false
$HasDefaultModel = $false
$HasAcpMode = $false
$ListenAddr = "127.0.0.1:8787"
$ForwardArgs = [System.Collections.Generic.List[string]]::new()

for ($i = 0; $i -lt $args.Count; $i++) {
    $tok = [string]$args[$i]
    switch ($tok) {
        "-Daemon"     { $Daemon = $true; continue }
        "--daemon"    { $Daemon = $true; continue }
        "-Stop"       { $Stop = $true; continue }
        "--stop"      { $Stop = $true; continue }
        "-SkipBuild"  { $SkipBuild = $true; continue }
        "--skip-build" { $SkipBuild = $true; continue }
        "-RebuildWeb" { $RebuildWeb = $true; continue }
        "--rebuild-web" { $RebuildWeb = $true; continue }
        default {
            if ($tok -eq "--listen") {
                $HasListen = $true
                $ForwardArgs.Add($tok)
                if ($i + 1 -lt $args.Count) {
                    $i++
                    $ListenAddr = [string]$args[$i]
                    $ForwardArgs.Add($ListenAddr)
                }
                continue
            }
            if ($tok.StartsWith("--listen=")) {
                $HasListen = $true
                $ListenAddr = $tok.Substring("--listen=".Length)
            }
            if ($tok -eq "--default-model" -or $tok.StartsWith("--default-model=")) {
                $HasDefaultModel = $true
            }
            if ($tok -eq "--acp-mode" -or $tok.StartsWith("--acp-mode=")) {
                $HasAcpMode = $true
            }
            $ForwardArgs.Add($tok)
        }
    }
}

if ($Stop) {
    if (Test-Path $PidFile) {
        $rawPid = (Get-Content $PidFile -ErrorAction SilentlyContinue | Select-Object -First 1)
        if ($rawPid -match '^\d+$') {
            $procId = [int]$rawPid
            $proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
            if ($null -ne $proc) {
                Stop-Process -Id $procId -Force -ErrorAction SilentlyContinue
                Write-Host "[STOP] Stopped liftgate (PID $procId)." -ForegroundColor Yellow
            }
        }
        Remove-Item $PidFile -Force -ErrorAction SilentlyContinue
    } else {
        Write-Host "[INFO] No background gateway PID file found at $PidFile" -ForegroundColor Yellow
    }
    return
}

function Resolve-BunExecutable {
    $cmd = Get-Command bun -ErrorAction SilentlyContinue
    if ($null -ne $cmd) { return $cmd.Source }
    $candidates = @(
        "C:\nvm4w\nodejs\node_modules\bun\bin\bun.exe",
        (Join-Path $HOME ".bun\bin\bun.exe"),
        (Join-Path $HOME ".bun/bin/bun")
    )
    foreach ($c in $candidates) {
        if (Test-Path $c) { return $c }
    }
    return $null
}

if (-not $SkipBuild) {
    $distIndex = Join-Path $RootDir "web\dist\index.html"
    if ($RebuildWeb -or -not (Test-Path $distIndex)) {
        $bunExe = Resolve-BunExecutable
        if ($null -ne $bunExe) {
            Write-Host "[1/3] Building companion web dashboard (web/dist) ..." -ForegroundColor Cyan
            $webDir = Join-Path $RootDir "web"
            & $bunExe install --cwd $webDir --frozen-lockfile
            if ($LASTEXITCODE -ne 0) { throw "bun install failed with exit code $LASTEXITCODE" }
            & $bunExe run --cwd $webDir build
            if ($LASTEXITCODE -ne 0) { throw "bun run build failed with exit code $LASTEXITCODE" }
        } else {
            Write-Host "[1/3] Bun not found; skipping web/dist build (API endpoints unaffected)." -ForegroundColor Yellow
        }
    } else {
        Write-Host "[1/3] Companion web bundle (web/dist) already built." -ForegroundColor Cyan
    }

    Write-Host "[2/3] Building release binary (cargo build --release --locked -p liftgate) ..." -ForegroundColor Cyan
    Push-Location $RootDir
    try {
        & cargo build --release --locked -p liftgate
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build --release failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
}

$binName = if ($IsWindows -or $env:OS -eq "Windows_NT") { "liftgate.exe" } else { "liftgate" }
$gwBin = Join-Path $RootDir "target\release\$binName"
if (-not (Test-Path $gwBin)) {
    throw "Release binary not found at $gwBin"
}

$finalArgs = [System.Collections.Generic.List[string]]::new()
if (-not $HasListen) {
    $finalArgs.Add("--listen")
    $finalArgs.Add($ListenAddr)
}
if (-not $HasDefaultModel) {
    $finalArgs.Add("--default-model")
    $finalArgs.Add("gemini-4-carbon-high")
}
if (-not $HasAcpMode) {
    $finalArgs.Add("--acp-mode")
    $finalArgs.Add("auto")
}
foreach ($a in $ForwardArgs) {
    $finalArgs.Add($a)
}

$baseUrl = "http://$ListenAddr"
Write-Host ""
Write-Host "  Gateway Base & Web UI   : $baseUrl/" -ForegroundColor Green
Write-Host "  OpenAI Responses API    : $baseUrl/v1/responses" -ForegroundColor White
Write-Host "  Anthropic Messages API  : $baseUrl/v1/messages" -ForegroundColor White
Write-Host "  Live Models & Catalog   : $baseUrl/v1/models  |  $baseUrl/v1/codex-models" -ForegroundColor White
Write-Host ""

if ($Daemon) {
    Write-Host "[3/3] Starting liftgate in background ..." -ForegroundColor Cyan
    $startParams = @{
        FilePath               = $gwBin
        ArgumentList           = $finalArgs
        WorkingDirectory       = $RootDir
        RedirectStandardOutput = $LogOut
        RedirectStandardError  = $LogErr
        PassThru               = $true
    }
    if ($IsWindows -or $env:OS -eq "Windows_NT") {
        $startParams["WindowStyle"] = "Hidden"
    }
    $proc = Start-Process @startParams
    Set-Content -Path $PidFile -Value "$($proc.Id)" -NoNewline

    $ready = $false
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 300
        if ($proc.HasExited) {
            $errTail = if (Test-Path $LogErr) { (Get-Content $LogErr -Tail 20) -join "`n" } else { "" }
            throw "liftgate exited unexpectedly (code $($proc.ExitCode)):`n$errTail"
        }
        try {
            $resp = Invoke-WebRequest -Uri "$baseUrl/healthz" -UseBasicParsing -TimeoutSec 2
            if ($resp.StatusCode -ge 200 -and $resp.StatusCode -lt 300) {
                $ready = $true
                break
            }
        } catch {}
    }
    if (-not $ready) {
        throw "Timed out waiting for $baseUrl/healthz"
    }
    Write-Host "[OK] Gateway daemon running (PID $($proc.Id)). Stop with: .\run-gateway.cmd -Stop" -ForegroundColor Green
    return
}

Write-Host "[3/3] Starting liftgate in foreground (press Ctrl+C to stop) ..." -ForegroundColor Cyan
& $gwBin @finalArgs
exit $LASTEXITCODE
