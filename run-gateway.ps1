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
$ProgressPreference = "SilentlyContinue"
$IsWin = ($IsWindows -or $env:OS -eq "Windows_NT")

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
$ShowHelp = $false
$HasListen = $false
$HasDefaultModel = $false
$HasAcpMode = $false
$ListenAddr = if (-not [string]::IsNullOrWhiteSpace($env:AGY_GATEWAY_LISTEN)) { $env:AGY_GATEWAY_LISTEN } else { "127.0.0.1:8787" }
$DefaultModel = if (-not [string]::IsNullOrWhiteSpace($env:AGY_DEFAULT_MODEL)) { $env:AGY_DEFAULT_MODEL } else { "gemini-4-carbon-high" }
$AcpMode = if (-not [string]::IsNullOrWhiteSpace($env:AGY_ACP_MODE)) { $env:AGY_ACP_MODE } else { "auto" }
$ForwardArgs = [System.Collections.Generic.List[string]]::new()

for ($i = 0; $i -lt $args.Count; $i++) {
    $tok = [string]$args[$i]
    switch ($tok) {
        "-h"            { $ShowHelp = $true; continue }
        "--help"        { $ShowHelp = $true; continue }
        "-Help"         { $ShowHelp = $true; continue }
        "-?"            { $ShowHelp = $true; continue }
        "/?"            { $ShowHelp = $true; continue }
        "-Daemon"       { $Daemon = $true; continue }
        "--daemon"      { $Daemon = $true; continue }
        "-Stop"         { $Stop = $true; continue }
        "--stop"        { $Stop = $true; continue }
        "-SkipBuild"    { $SkipBuild = $true; continue }
        "--skip-build"  { $SkipBuild = $true; continue }
        "-RebuildWeb"   { $RebuildWeb = $true; continue }
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

if ($ShowHelp) {
    Write-Host "Usage: .\run-gateway.cmd [script-options] [liftgate-flags...]"
    Write-Host ""
    Write-Host "Script options:"
    Write-Host "  -Daemon, --daemon          Build release and run liftgate in the background"
    Write-Host "  -Stop, --stop              Stop background gateway (.agy-runtime/gateway.pid)"
    Write-Host "  -SkipBuild, --skip-build   Start existing release binary without building"
    Write-Host "  -RebuildWeb, --rebuild-web Force rebuild of web/dist via Bun"
    Write-Host "  -h, --help                 Show this help message"
    Write-Host ""
    Write-Host "Any other flags (--listen, --default-model, --acp-mode, --api-key, --agy-bin, ...) pass through to liftgate."
    return
}

function Stop-ProcessTree([int]$ProcId) {
    if ($IsWin) {
        & taskkill /PID $ProcId /T /F >$null 2>&1
    }
    Stop-Process -Id $ProcId -Force -ErrorAction SilentlyContinue
}

function Format-Win32Arg([string]$Value) {
    if ($null -eq $Value) { return '""' }
    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') {
        return $Value
    }
    $escaped = ($Value -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1'
    return "`"$escaped`""
}

function Join-Win32Args([System.Collections.Generic.IEnumerable[string]]$Values) {
    $parts = [System.Collections.Generic.List[string]]::new()
    foreach ($v in $Values) {
        $parts.Add((Format-Win32Arg $v))
    }
    return ($parts -join " ")
}

if ($Stop) {
    if (Test-Path $PidFile) {
        $rawPid = (Get-Content $PidFile -ErrorAction SilentlyContinue | Select-Object -First 1)
        if ($rawPid -match '^\d+$') {
            $procId = [int]$rawPid
            $proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
            if ($null -ne $proc) {
                Stop-ProcessTree -ProcId $procId
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
    if ($IsWin) {
        $exeCmd = Get-Command bun.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $exeCmd -and -not [string]::IsNullOrWhiteSpace($exeCmd.Source) -and (Test-Path $exeCmd.Source)) {
            return $exeCmd.Source
        }
        foreach ($c in (Get-Command bun -All -ErrorAction SilentlyContinue)) {
            if (-not [string]::IsNullOrWhiteSpace($c.Source)) {
                $shimDir = Split-Path -Parent $c.Source
                $nestedExe = Join-Path $shimDir "node_modules\bun\bin\bun.exe"
                if (Test-Path $nestedExe) {
                    return $nestedExe
                }
            }
        }
        $winCandidates = @(
            (Join-Path $HOME ".bun\bin\bun.exe"),
            $(if ($env:USERPROFILE) { Join-Path $env:USERPROFILE ".bun\bin\bun.exe" }),
            $(if ($env:APPDATA) { Join-Path $env:APPDATA "npm\node_modules\bun\bin\bun.exe" }),
            $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Links\bun.exe" }),
            "C:\nvm4w\nodejs\node_modules\bun\bin\bun.exe"
        )
        foreach ($cand in $winCandidates) {
            if (-not [string]::IsNullOrWhiteSpace($cand) -and (Test-Path $cand)) {
                return $cand
            }
        }
        $cmdShim = Get-Command bun.cmd,bun.bat -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $cmdShim -and (Test-Path $cmdShim.Source)) {
            return $cmdShim.Source
        }
        return $null
    }

    $unixCmd = Get-Command bun -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $unixCmd -and (Test-Path $unixCmd.Source)) {
        return $unixCmd.Source
    }
    foreach ($cand in @((Join-Path $HOME ".bun/bin/bun"), "/opt/homebrew/bin/bun", "/usr/local/bin/bun")) {
        if (Test-Path $cand) {
            return $cand
        }
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

$binName = if ($IsWin) { "liftgate.exe" } else { "liftgate" }
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
    $finalArgs.Add($DefaultModel)
}
if (-not $HasAcpMode) {
    $finalArgs.Add("--acp-mode")
    $finalArgs.Add($AcpMode)
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
    $savedNoProxy = $env:NO_PROXY
    $env:NO_PROXY = "127.0.0.1,localhost,::1"
    try {
        $startParams = @{
            FilePath               = $gwBin
            WorkingDirectory       = $RootDir
            RedirectStandardOutput = $LogOut
            RedirectStandardError  = $LogErr
            PassThru               = $true
        }
        if ($finalArgs.Count -gt 0) {
            $startParams["ArgumentList"] = (Join-Win32Args $finalArgs)
        }
        if ($IsWin) {
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
    } finally {
        if ($null -eq $savedNoProxy) {
            Remove-Item Env:\NO_PROXY -ErrorAction SilentlyContinue
        } else {
            $env:NO_PROXY = $savedNoProxy
        }
    }
    return
}

Write-Host "[3/3] Starting liftgate in foreground (press Ctrl+C to stop) ..." -ForegroundColor Cyan
& $gwBin @finalArgs
exit $LASTEXITCODE
