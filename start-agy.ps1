#!/usr/bin/env pwsh
<#
.SYNOPSIS
  一键启动 Liftgate + 实时监控前端 + 动态模型发现 + 本地环境变量注入 + Claude CLI (Gemini Carbon)

.EXAMPLES
  # 一键启动网关、前端、同步全部模型发现并直接进入交互式 Claude CLI (默认 gemini-4-carbon-high)
  .\start-agy.ps1

  # 仅在后台启动网关 + 前端 + 刷新模型发现与本地环境变量（不进入交互式 Claude CLI）
  .\start-agy.ps1 -BackgroundOnly

  # 将网关环境变量持久化写入当前系统用户级环境变量（后续任意终端直接敲 claude / codex 均走本地网关）
  .\start-agy.ps1 -BackgroundOnly -InstallEnv

  # 清理已持久化的用户级环境变量
  .\start-agy.ps1 -UninstallEnv

  # 停止后台运行的网关与前端进程
  .\start-agy.ps1 -StopServices

  # 直接透传任意参数给 Claude CLI（例如 -p 单次执行、-c 续接上次会话、-r 恢复历史会话）
  .\start-agy.ps1 -p "你好，请介绍一下你当前的模型"

  # 一键启动服务并以本地网关配置启动 OpenAI Codex CLI (/v1/responses)
  .\start-agy.ps1 -Codex
#>
$ErrorActionPreference = "Stop"

$GatewayModel = "gemini-4-carbon-high"
$FastModel = "gemini-3.8-flash-low"
$ExplicitFastModel = $false
$ListenHost = "127.0.0.1"
$ListenPort = 8787
$UiPort = 5173
$ApiKey = "local-agy"
$BackgroundOnly = $false
$InstallEnv = $false
$UninstallEnv = $false
$StopServices = $false
$LaunchCodex = $false
$ClaudeArgs = [System.Collections.Generic.List[string]]::new()

for ($idx = 0; $idx -lt $args.Count; $idx++) {
    $tok = [string]$args[$idx]
    switch ($tok) {
        "-GatewayModel" { $idx++; if ($idx -lt $args.Count) { $GatewayModel = [string]$args[$idx] }; continue }
        "-FastModel"    { $idx++; if ($idx -lt $args.Count) { $FastModel = [string]$args[$idx]; $ExplicitFastModel = $true }; continue }
        "-ListenHost"   { $idx++; if ($idx -lt $args.Count) { $ListenHost = [string]$args[$idx] }; continue }
        "-ListenPort"   { $idx++; if ($idx -lt $args.Count) { $ListenPort = [int]$args[$idx] }; continue }
        "-UiPort"       { $idx++; if ($idx -lt $args.Count) { $UiPort = [int]$args[$idx] }; continue }
        "-ApiKey"       { $idx++; if ($idx -lt $args.Count) { $ApiKey = [string]$args[$idx] }; continue }
        "-BackgroundOnly" { $BackgroundOnly = $true; continue }
        "-InstallEnv"     { $InstallEnv = $true; continue }
        "-UninstallEnv"   { $UninstallEnv = $true; continue }
        "-StopServices"   { $StopServices = $true; continue }
        "-Codex"          { $LaunchCodex = $true; continue }
        default           { $ClaudeArgs.Add($tok) }
    }
}

$RootDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RuntimeDir = Join-Path $RootDir ".agy-runtime"
New-Item -ItemType Directory -Force -Path $RuntimeDir | Out-Null

$GatewayPidFile = Join-Path $RuntimeDir "gateway.pid"
$WebPidFile = Join-Path $RuntimeDir "web.pid"
$GatewayLogOut = Join-Path $RuntimeDir "gateway.out.log"
$GatewayLogErr = Join-Path $RuntimeDir "gateway.err.log"
$WebLogOut = Join-Path $RuntimeDir "web.out.log"
$WebLogErr = Join-Path $RuntimeDir "web.err.log"
$SettingsPath = Join-Path $RootDir "claude-carbon.settings.json"
$LocalClaudeDir = Join-Path $RootDir ".claude"
$LocalSettingsPath = Join-Path $LocalClaudeDir "settings.local.json"

$GatewayBaseUrl = "http://${ListenHost}:${ListenPort}"
$WebBaseUrl = "http://${ListenHost}:${UiPort}"

$EnvKeys = @(
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_AUTO_MODE_SERVER",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "NO_PROXY",
    "AGY_API_KEY",
    "AGY_GATEWAY_API_KEY"
)

if ($UninstallEnv) {
    if ($IsWindows -or $env:OS -eq "Windows_NT") {
        foreach ($k in $EnvKeys) {
            Remove-ItemProperty -Path "HKCU:\Environment" -Name $k -ErrorAction SilentlyContinue
        }
        Write-Host "[OK] Cleared persisted User-level gateway environment variables." -ForegroundColor Green
    } else {
        Write-Host "[INFO] On Linux/macOS, remove the sourced .agy-runtime/env.sh line from your shell rc if added." -ForegroundColor Yellow
    }
    return
}

function Stop-TrackedProcess([string]$PidFile, [string]$Label) {
    if (Test-Path $PidFile) {
        $rawPid = (Get-Content $PidFile -ErrorAction SilentlyContinue | Select-Object -First 1)
        if ($rawPid -match '^\d+$') {
            $procId = [int]$rawPid
            $proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
            if ($null -ne $proc) {
                Stop-Process -Id $procId -Force -ErrorAction SilentlyContinue
                Write-Host "[STOP] Stopped $Label (PID $procId)" -ForegroundColor Yellow
            }
        }
        Remove-Item $PidFile -Force -ErrorAction SilentlyContinue
    }
}

if ($StopServices) {
    Stop-TrackedProcess -PidFile $WebPidFile -Label "Web Frontend"
    Stop-TrackedProcess -PidFile $GatewayPidFile -Label "liftgate"
    return
}

function Test-HttpOk([string]$Url) {
    try {
        $resp = Invoke-WebRequest -Uri $Url -UseBasicParsing -TimeoutSec 2
        return ($resp.StatusCode -ge 200 -and $resp.StatusCode -lt 300)
    } catch {
        return $false
    }
}

function Resolve-BunExecutable {
    $cmd = Get-Command bun -ErrorAction SilentlyContinue
    if ($null -ne $cmd) {
        return $cmd.Source
    }
    $candidates = @(
        "C:\nvm4w\nodejs\node_modules\bun\bin\bun.exe",
        (Join-Path $HOME ".bun\bin\bun.exe"),
        (Join-Path $HOME ".bun/bin/bun")
    )
    foreach ($c in $candidates) {
        if (Test-Path $c) {
            return $c
        }
    }
    return $null
}

# ============================================================================
# 1. Ensure liftgate is running
# ============================================================================
if (Test-HttpOk "$GatewayBaseUrl/healthz") {
    Write-Host "[1/4] Gateway already running at $GatewayBaseUrl" -ForegroundColor Cyan
} else {
    $binName = if ($IsWindows -or $env:OS -eq "Windows_NT") { "liftgate.exe" } else { "liftgate" }
    $gwBin = Join-Path $RootDir "target\release\$binName"
    if (-not (Test-Path $gwBin)) {
        Write-Host "[1/4] Building release liftgate ..." -ForegroundColor Cyan
        Push-Location $RootDir
        try {
            & cargo build --release -p liftgate
            if ($LASTEXITCODE -ne 0) {
                throw "cargo build failed with exit code $LASTEXITCODE"
            }
        } finally {
            Pop-Location
        }
    }

    Write-Host "[1/4] Starting liftgate on $GatewayBaseUrl (default model: $GatewayModel) ..." -ForegroundColor Cyan
    $gwArgs = @("--listen", "${ListenHost}:${ListenPort}", "--default-model", $GatewayModel, "--acp-mode", "auto")
    $startParams = @{
        FilePath               = $gwBin
        ArgumentList           = $gwArgs
        WorkingDirectory       = $RootDir
        RedirectStandardOutput = $GatewayLogOut
        RedirectStandardError  = $GatewayLogErr
        PassThru               = $true
    }
    if ($IsWindows -or $env:OS -eq "Windows_NT") {
        $startParams["WindowStyle"] = "Hidden"
    }
    $gwProc = Start-Process @startParams
    Set-Content -Path $GatewayPidFile -Value "$($gwProc.Id)" -NoNewline

    $ready = $false
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Milliseconds 300
        if ($gwProc.HasExited) {
            $errTail = if (Test-Path $GatewayLogErr) { (Get-Content $GatewayLogErr -Tail 20) -join "`n" } else { "" }
            throw "liftgate exited unexpectedly (code $($gwProc.ExitCode)):`n$errTail"
        }
        if (Test-HttpOk "$GatewayBaseUrl/healthz") {
            $ready = $true
            break
        }
    }
    if (-not $ready) {
        throw "Timed out waiting for $GatewayBaseUrl/healthz"
    }
    Write-Host "      Gateway online (PID $($gwProc.Id)) -> $GatewayBaseUrl/" -ForegroundColor Green
}

# ============================================================================
# 2. Ensure Companion Web Frontend (Bun + Vite) is running
# ============================================================================
if (Test-HttpOk "$WebBaseUrl/") {
    Write-Host "[2/4] Companion frontend already running at $WebBaseUrl/" -ForegroundColor Cyan
} else {
    $bunExe = Resolve-BunExecutable
    if ($null -eq $bunExe) {
        Write-Host "[2/4] Bun not found in PATH; embedded UI is still live at $GatewayBaseUrl/" -ForegroundColor Yellow
    } else {
        $nodeModulesDir = Join-Path $RootDir "web\node_modules"
        if (-not (Test-Path $nodeModulesDir)) {
            Write-Host "[2/4] Installing frontend dependencies via Bun ..." -ForegroundColor Cyan
            & $bunExe install --cwd (Join-Path $RootDir "web")
        }
        Write-Host "[2/4] Starting companion frontend on $WebBaseUrl/ ..." -ForegroundColor Cyan
        $env:VITE_GATEWAY_PROXY = $GatewayBaseUrl
        $webDir = Join-Path $RootDir "web"
        $webParams = @{
            FilePath               = $bunExe
            ArgumentList           = @("run", "--cwd", $webDir, "dev", "--host", $ListenHost, "--port", "$UiPort")
            WorkingDirectory       = $webDir
            RedirectStandardOutput = $WebLogOut
            RedirectStandardError  = $WebLogErr
            PassThru               = $true
        }
        if ($IsWindows -or $env:OS -eq "Windows_NT") {
            $webParams["WindowStyle"] = "Hidden"
        }
        $webProc = Start-Process @webParams
        Set-Content -Path $WebPidFile -Value "$($webProc.Id)" -NoNewline

        $webReady = $false
        for ($i = 0; $i -lt 30; $i++) {
            Start-Sleep -Milliseconds 300
            if (Test-HttpOk "$WebBaseUrl/") {
                $webReady = $true
                break
            }
        }
        if ($webReady) {
            Write-Host "      Frontend online (PID $($webProc.Id)) -> $WebBaseUrl/" -ForegroundColor Green
        } else {
            Write-Host "      Frontend dev server still warming up; embedded UI ready at $GatewayBaseUrl/" -ForegroundColor Yellow
        }
    }
}

function Resolve-AgyExecutable {
    $cmd = Get-Command agy -ErrorAction SilentlyContinue
    if ($null -ne $cmd) {
        return $cmd.Source
    }
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "agy\bin\agy.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\Antigravity\bin\agy.exe"),
        (Join-Path $HOME ".local/bin/agy"),
        "/opt/homebrew/bin/agy",
        "/usr/local/bin/agy"
    )
    foreach ($c in $candidates) {
        if (-not [string]::IsNullOrWhiteSpace($c) -and (Test-Path $c)) {
            return $c
        }
    }
    return $null
}

function Get-WindowsSystemProxyUrl {
    if (-not ($IsWindows -or $env:OS -eq "Windows_NT")) {
        return $null
    }
    try {
        $inet = Get-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Internet Settings" -ErrorAction SilentlyContinue
        if ($null -ne $inet -and $inet.ProxyEnable -eq 1 -and -not [string]::IsNullOrWhiteSpace($inet.ProxyServer)) {
            $srv = [string]$inet.ProxyServer
            if ($srv.Contains("=") -or $srv.Contains(";")) {
                foreach ($part in ($srv -split ";")) {
                    if ($part -match '^https=(.+)$') {
                        $srv = $Matches[1]
                        break
                    }
                }
            }
            if ($srv -notmatch '://') {
                return "http://$srv"
            }
            return $srv
        }
    } catch {}
    return $null
}

# ============================================================================
# 3. Dynamic Model Discovery (/v1/models) -> Claude Code modelPicker & Overrides
# ============================================================================
Write-Host "[3/4] Discovering live models from $GatewayBaseUrl/v1/models ..." -ForegroundColor Cyan
$env:NO_PROXY = "127.0.0.1,localhost,::1"
$modelsResp = Invoke-RestMethod -Uri "$GatewayBaseUrl/v1/models" -Headers @{ Authorization = "Bearer $ApiKey" } -TimeoutSec 15

if ($modelsResp.data.Count -le 1) {
    $agyExe = Resolve-AgyExecutable
    if ($null -ne $agyExe) {
        Write-Host "      Antigravity CLI requires Google sign-in; launching 'agy acp --login' ..." -ForegroundColor Yellow
        $sysProxy = Get-WindowsSystemProxyUrl
        if ($null -ne $sysProxy -and [string]::IsNullOrWhiteSpace($env:HTTPS_PROXY)) {
            $env:HTTP_PROXY = $sysProxy
            $env:HTTPS_PROXY = $sysProxy
        }
        Remove-Item Env:\AGY_API_KEY -ErrorAction SilentlyContinue
        & $agyExe acp --login
        $modelsResp = Invoke-RestMethod -Uri "$GatewayBaseUrl/v1/models" -Headers @{ Authorization = "Bearer $ApiKey" } -TimeoutSec 15
    }
}

$pickerOptions = [System.Collections.Generic.List[object]]::new()

foreach ($m in $modelsResp.data) {
    $id = [string]$m.id
    $display = if (-not [string]::IsNullOrWhiteSpace($m.display_name)) { [string]$m.display_name } else { $id }
    $ctxK = if ($null -ne $m.context_window -and [long]$m.context_window -gt 0) {
        "$([math]::Round([double]$m.context_window / 1000))k ctx"
    } else {
        "1000k ctx"
    }
    $lower = $id.ToLowerInvariant()
    $behavesAs = if ($lower.Contains("flash-low")) {
        "claude-haiku-4-5-20251001"
    } elseif ($lower.Contains("sonnet") -or $lower.EndsWith("-medium")) {
        "claude-sonnet-4-6"
    } else {
        "claude-opus-4-6"
    }

    $pickerOptions.Add([ordered]@{
        model       = $id
        label       = $display
        description = "$ctxK · Antigravity ACP ($id)"
        behavesAs   = $behavesAs
    })
}

$detectedProxy = if (-not [string]::IsNullOrWhiteSpace($env:HTTPS_PROXY)) {
    $env:HTTPS_PROXY
} elseif (-not [string]::IsNullOrWhiteSpace($env:HTTP_PROXY)) {
    $env:HTTP_PROXY
} else {
    Get-WindowsSystemProxyUrl
}

if (-not $ExplicitFastModel) {
    $flashCandidates = @(
        $modelsResp.data |
            ForEach-Object { [string]$_.id } |
            Where-Object { $_ -match '^gemini-.*flash.*-low$' } |
            Sort-Object -Descending
    )
    if ($flashCandidates.Count -gt 0) {
        $FastModel = $flashCandidates[0]
    }
}

$envMap = [ordered]@{
    ANTHROPIC_BASE_URL                        = $GatewayBaseUrl
    ANTHROPIC_AUTH_TOKEN                      = $ApiKey
    ANTHROPIC_API_KEY                         = ""
    ANTHROPIC_MODEL                           = $GatewayModel
    ANTHROPIC_DEFAULT_OPUS_MODEL              = $GatewayModel
    ANTHROPIC_DEFAULT_SONNET_MODEL            = $GatewayModel
    ANTHROPIC_DEFAULT_HAIKU_MODEL             = $FastModel
    ANTHROPIC_SMALL_FAST_MODEL                = $FastModel
    CLAUDE_CODE_MAX_CONTEXT_TOKENS            = "1000000"
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC  = "1"
    CLAUDE_CODE_AUTO_MODE_SERVER              = "0"
    NO_PROXY                                  = "127.0.0.1,localhost,::1"
}
if (-not [string]::IsNullOrWhiteSpace($detectedProxy)) {
    $envMap["HTTP_PROXY"]  = $detectedProxy
    $envMap["HTTPS_PROXY"] = $detectedProxy
}

$settingsObj = [ordered]@{
    model        = $GatewayModel
    env          = $envMap
    modelOverrides = [ordered]@{
        "claude-opus-4-6"           = $GatewayModel
        "claude-sonnet-4-6"         = $GatewayModel
        "claude-haiku-4-5-20251001" = $FastModel
    }
    modelPicker  = [ordered]@{
        replaceBuiltInOptions = $true
        options               = $pickerOptions
    }
}

$utf8NoBom = [System.Text.UTF8Encoding]::new($false)
$settingsJson = $settingsObj | ConvertTo-Json -Depth 10
[System.IO.File]::WriteAllText($SettingsPath, $settingsJson, $utf8NoBom)
New-Item -ItemType Directory -Force -Path $LocalClaudeDir | Out-Null
[System.IO.File]::WriteAllText($LocalSettingsPath, $settingsJson, $utf8NoBom)

$codexCatalogPath = Join-Path $RuntimeDir "codex-model-catalog.json"
$codexCatalogSlash = $codexCatalogPath.Replace('\', '/')
$encodedDefaultModel = [uri]::EscapeDataString($GatewayModel)
Invoke-WebRequest -Uri "$GatewayBaseUrl/v1/codex-models?default_model=$encodedDefaultModel" -Headers @{ Authorization = "Bearer $ApiKey" } -UseBasicParsing -TimeoutSec 15 -OutFile $codexCatalogPath

$codexConfigPath = Join-Path $RuntimeDir "codex-config.toml"
$codexToml = @"
model = "$GatewayModel"
model_provider = "agy"
model_catalog_json = "$codexCatalogSlash"
web_search = "live"
model_context_window = 1000000
model_auto_compact_token_limit = 850000

[model_providers.agy]
name = "Liftgate"
base_url = "$GatewayBaseUrl/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"
"@
[System.IO.File]::WriteAllText($codexConfigPath, $codexToml, $utf8NoBom)

Write-Host "      Synced $($pickerOptions.Count) live models into Claude modelPicker & Codex model_catalog_json (.agy-runtime/codex-model-catalog.json)" -ForegroundColor Green

# ============================================================================
# 4. Configure Local Process & Optional Persisted User Environment Variables
#    (Snapshot caller PowerShell Env:\ first so interactive CLI exit restores
#     the exact original shell environment without polluting normal codex/claude!)
# ============================================================================
$TrackedSessionEnvKeys = @(
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_AUTO_MODE_SERVER",
    "AGY_API_KEY",
    "AGY_GATEWAY_API_KEY",
    "NO_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY"
)
$SavedProcessEnv = @{}
foreach ($k in $TrackedSessionEnvKeys) {
    $existing = Get-Item -Path "Env:\$k" -ErrorAction SilentlyContinue
    $SavedProcessEnv[$k] = if ($null -ne $existing) { [string]$existing.Value } else { $null }
}

function Restore-CallerProcessEnv {
    foreach ($entry in $SavedProcessEnv.GetEnumerator()) {
        if ($null -eq $entry.Value) {
            Remove-Item -Path "Env:\$($entry.Key)" -ErrorAction SilentlyContinue
        } else {
            Set-Item -Path "Env:\$($entry.Key)" -Value $entry.Value
        }
    }
}

Remove-Item Env:\ANTHROPIC_API_KEY -ErrorAction SilentlyContinue
Remove-Item Env:\AGY_API_KEY -ErrorAction SilentlyContinue
if ($InstallEnv -and ($IsWindows -or $env:OS -eq "Windows_NT")) {
    Remove-ItemProperty -Path "HKCU:\Environment" -Name "ANTHROPIC_API_KEY" -ErrorAction SilentlyContinue
    Remove-ItemProperty -Path "HKCU:\Environment" -Name "AGY_API_KEY" -ErrorAction SilentlyContinue
    # Never hijack default OpenAI/Codex base_url or api_key globally; Codex uses dedicated AGY_GATEWAY_API_KEY via -Codex
    $curOpenAiKey = (Get-ItemProperty -Path "HKCU:\Environment" -ErrorAction SilentlyContinue).OPENAI_API_KEY
    if ($curOpenAiKey -eq $ApiKey) {
        Remove-ItemProperty -Path "HKCU:\Environment" -Name "OPENAI_API_KEY" -ErrorAction SilentlyContinue
        Remove-ItemProperty -Path "HKCU:\Environment" -Name "OPENAI_BASE_URL" -ErrorAction SilentlyContinue
    }
}

# Note: OPENAI_BASE_URL and OPENAI_API_KEY are intentionally NOT set here so
# running plain `codex` outside `-Codex` always preserves your original Codex config!
$fullEnvMap = [ordered]@{
    ANTHROPIC_BASE_URL                        = $GatewayBaseUrl
    ANTHROPIC_AUTH_TOKEN                      = $ApiKey
    ANTHROPIC_MODEL                           = $GatewayModel
    ANTHROPIC_DEFAULT_OPUS_MODEL              = $GatewayModel
    ANTHROPIC_DEFAULT_SONNET_MODEL            = $GatewayModel
    ANTHROPIC_DEFAULT_HAIKU_MODEL             = $FastModel
    ANTHROPIC_SMALL_FAST_MODEL                = $FastModel
    CLAUDE_CODE_MAX_CONTEXT_TOKENS            = "1000000"
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC  = "1"
    CLAUDE_CODE_AUTO_MODE_SERVER              = "0"
    AGY_GATEWAY_API_KEY                       = $ApiKey
    NO_PROXY                                  = "127.0.0.1,localhost,::1"
}
if (-not [string]::IsNullOrWhiteSpace($detectedProxy)) {
    $env:HTTP_PROXY  = $detectedProxy
    $env:HTTPS_PROXY = $detectedProxy
}

foreach ($entry in $fullEnvMap.GetEnumerator()) {
    Set-Item -Path "Env:\$($entry.Key)" -Value $entry.Value
    if ($InstallEnv -and ($IsWindows -or $env:OS -eq "Windows_NT")) {
        Set-ItemProperty -Path "HKCU:\Environment" -Name $entry.Key -Value $entry.Value
    }
}

$shExportLines = @(
    "unset ANTHROPIC_API_KEY",
    "unset AGY_API_KEY"
) + @(foreach ($entry in $fullEnvMap.GetEnumerator()) {
    "export $($entry.Key)=`"$($entry.Value)`""
})
[System.IO.File]::WriteAllLines((Join-Path $RuntimeDir "env.sh"), $shExportLines, $utf8NoBom)

if ($InstallEnv -and ($IsWindows -or $env:OS -eq "Windows_NT")) {
    Write-Host "[4/4] Persisted gateway environment variables to Windows User profile (isolated AGY_GATEWAY_API_KEY for Codex)." -ForegroundColor Green
} else {
    Write-Host "[4/4] Scoped environment ready (restores original shell env automatically upon CLI exit)." -ForegroundColor Green
}

Write-Host ""
Write-Host "  Gateway API & Embedded UI : $GatewayBaseUrl/" -ForegroundColor White
Write-Host "  Companion Dev Frontend    : $WebBaseUrl/" -ForegroundColor White
Write-Host "  Models Discovery Endpoint : $GatewayBaseUrl/v1/models ($($pickerOptions.Count) models)" -ForegroundColor White
Write-Host "  Active Primary Model      : $GatewayModel (Fast/Subagent: $FastModel)" -ForegroundColor White
Write-Host ""

if ($BackgroundOnly) {
    return
}

$cliExitCode = 0
try {
    if ($LaunchCodex) {
        $providerSpec = "{name=`"Liftgate`",base_url=`"$GatewayBaseUrl/v1`",env_key=`"AGY_GATEWAY_API_KEY`",wire_api=`"responses`"}"
        $hasExplicitCodexModel = $false
        foreach ($ca in $ClaudeArgs) {
            if ($ca -eq "-m" -or $ca -eq "--model" -or $ca.StartsWith("-m=") -or $ca.StartsWith("--model=")) {
                $hasExplicitCodexModel = $true
                break
            }
        }
        if ($hasExplicitCodexModel) {
            & codex -c "model_provider=`"agy`"" -c "model_catalog_json=`"$codexCatalogSlash`"" -c "web_search=`"live`"" -c "model_context_window=1000000" -c "model_auto_compact_token_limit=850000" -c "model_providers.agy=$providerSpec" @ClaudeArgs
        } else {
            & codex -c "model_provider=`"agy`"" -c "model=`"$GatewayModel`"" -c "model_catalog_json=`"$codexCatalogSlash`"" -c "web_search=`"live`"" -c "model_context_window=1000000" -c "model_auto_compact_token_limit=850000" -c "model_providers.agy=$providerSpec" @ClaudeArgs
        }
        $cliExitCode = $LASTEXITCODE
    } else {
        $hasExplicitModel = $false
        foreach ($ca in $ClaudeArgs) {
            if ($ca -eq "--model" -or $ca.StartsWith("--model=")) {
                $hasExplicitModel = $true
                break
            }
        }

        if ($hasExplicitModel) {
            & claude --setting-sources "project,local" --settings $SettingsPath @ClaudeArgs
        } else {
            & claude --setting-sources "project,local" --settings $SettingsPath --model $GatewayModel @ClaudeArgs
        }
        $cliExitCode = $LASTEXITCODE
    }
} finally {
    if (-not $InstallEnv) {
        Restore-CallerProcessEnv
    }
}
exit $cliExitCode
