#!/usr/bin/env pwsh
<#
.SYNOPSIS
  One-command launcher for Liftgate + companion dashboard + live model discovery + Claude Code / Codex CLI (full bypass enabled by default).

.EXAMPLES
  # Start gateway, frontend, sync models, and launch interactive Claude Code in full bypass mode
  .\start-agy.cmd

  # Start gateway, frontend, sync models, and launch OpenAI Codex CLI in full bypass mode
  .\start-agy.cmd -Codex

  # Launch without automatic permission/sandbox bypass
  .\start-agy.cmd -NoBypass

  # Start background services and refresh local model configs only (do not open CLI)
  .\start-agy.cmd -BackgroundOnly

  # Persist gateway env vars into Windows User profile
  .\start-agy.cmd -BackgroundOnly -InstallEnv

  # Remove persisted Windows User profile gateway env vars
  .\start-agy.cmd -UninstallEnv

  # Stop background gateway and frontend processes
  .\start-agy.cmd -StopServices

  # Forward arbitrary flags to Claude Code or Codex CLI
  .\start-agy.cmd -p "Hello, what model are you running on?"
#>
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$IsWin = ($IsWindows -or $env:OS -eq "Windows_NT")

$GatewayModel = if (-not [string]::IsNullOrWhiteSpace($env:AGY_MODEL)) { $env:AGY_MODEL } else { "gemini-4-carbon-high" }
$FastModel = if (-not [string]::IsNullOrWhiteSpace($env:AGY_FAST_MODEL)) { $env:AGY_FAST_MODEL } else { "gemini-4-hydrogen-low" }
$ExplicitFastModel = (-not [string]::IsNullOrWhiteSpace($env:AGY_FAST_MODEL))
$ListenHost = if (-not [string]::IsNullOrWhiteSpace($env:AGY_HOST)) { $env:AGY_HOST } else { "127.0.0.1" }
$ListenPort = if (-not [string]::IsNullOrWhiteSpace($env:AGY_PORT)) { [int]$env:AGY_PORT } else { 8787 }
$UiPort = if (-not [string]::IsNullOrWhiteSpace($env:AGY_WEB_PORT)) { [int]$env:AGY_WEB_PORT } else { 5173 }
$ApiKey = if (-not [string]::IsNullOrWhiteSpace($env:AGY_API_KEY)) { $env:AGY_API_KEY } else { "local-agy" }
$ExplicitApiKey = (-not [string]::IsNullOrWhiteSpace($env:AGY_API_KEY))
$BackgroundOnly = $false
$InstallEnv = $false
$UninstallEnv = $false
$StopServices = $false
$LaunchCodex = $false
$NoBypass = $false
$ShowHelp = $false
$ClaudeArgs = [System.Collections.Generic.List[string]]::new()

for ($idx = 0; $idx -lt $args.Count; $idx++) {
    $tok = [string]$args[$idx]
    if ($tok -eq "--") {
        for ($j = $idx + 1; $j -lt $args.Count; $j++) {
            $ClaudeArgs.Add([string]$args[$j])
        }
        break
    }
    switch ($tok) {
        "-h"                 { if ($ClaudeArgs.Count -eq 0) { $ShowHelp = $true } else { $ClaudeArgs.Add($tok) }; continue }
        "--help"             { if ($ClaudeArgs.Count -eq 0) { $ShowHelp = $true } else { $ClaudeArgs.Add($tok) }; continue }
        "-Help"              { $ShowHelp = $true; continue }
        "-?"                 { if ($ClaudeArgs.Count -eq 0) { $ShowHelp = $true } else { $ClaudeArgs.Add($tok) }; continue }
        "/?"                 { if ($ClaudeArgs.Count -eq 0) { $ShowHelp = $true } else { $ClaudeArgs.Add($tok) }; continue }
        "-GatewayModel"      { $idx++; if ($idx -lt $args.Count) { $GatewayModel = [string]$args[$idx] }; continue }
        "--gateway-model"    { $idx++; if ($idx -lt $args.Count) { $GatewayModel = [string]$args[$idx] }; continue }
        "-FastModel"         { $idx++; if ($idx -lt $args.Count) { $FastModel = [string]$args[$idx]; $ExplicitFastModel = $true }; continue }
        "--fast-model"       { $idx++; if ($idx -lt $args.Count) { $FastModel = [string]$args[$idx]; $ExplicitFastModel = $true }; continue }
        "-ListenHost"        { $idx++; if ($idx -lt $args.Count) { $ListenHost = [string]$args[$idx] }; continue }
        "--host"             { $idx++; if ($idx -lt $args.Count) { $ListenHost = [string]$args[$idx] }; continue }
        "-ListenPort"        { $idx++; if ($idx -lt $args.Count) { $ListenPort = [int]$args[$idx] }; continue }
        "-Port"              { $idx++; if ($idx -lt $args.Count) { $ListenPort = [int]$args[$idx] }; continue }
        "--port"             { $idx++; if ($idx -lt $args.Count) { $ListenPort = [int]$args[$idx] }; continue }
        "-UiPort"            { $idx++; if ($idx -lt $args.Count) { $UiPort = [int]$args[$idx] }; continue }
        "--ui-port"          { $idx++; if ($idx -lt $args.Count) { $UiPort = [int]$args[$idx] }; continue }
        "--web-port"         { $idx++; if ($idx -lt $args.Count) { $UiPort = [int]$args[$idx] }; continue }
        "-ApiKey"            { $idx++; if ($idx -lt $args.Count) { $ApiKey = [string]$args[$idx]; $ExplicitApiKey = $true }; continue }
        "--api-key"          { $idx++; if ($idx -lt $args.Count) { $ApiKey = [string]$args[$idx]; $ExplicitApiKey = $true }; continue }
        "-BackgroundOnly"    { $BackgroundOnly = $true; continue }
        "--background-only"  { $BackgroundOnly = $true; continue }
        "--services-only"    { $BackgroundOnly = $true; continue }
        "-InstallEnv"        { $InstallEnv = $true; continue }
        "--install-env"      { $InstallEnv = $true; continue }
        "-UninstallEnv"      { $UninstallEnv = $true; continue }
        "--uninstall-env"    { $UninstallEnv = $true; continue }
        "-StopServices"      { $StopServices = $true; continue }
        "-Stop"              { $StopServices = $true; continue }
        "--stop"             { $StopServices = $true; continue }
        "--stop-services"    { $StopServices = $true; continue }
        "-Codex"             { $LaunchCodex = $true; continue }
        "--codex"            { $LaunchCodex = $true; continue }
        "-NoBypass"          { $NoBypass = $true; continue }
        "--no-bypass"        { $NoBypass = $true; continue }
        default {
            if ($tok.StartsWith("--gateway-model=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $GatewayModel = $tok.Substring("--gateway-model=".Length)
                continue
            }
            if ($tok.StartsWith("--fast-model=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $FastModel = $tok.Substring("--fast-model=".Length)
                $ExplicitFastModel = $true
                continue
            }
            if ($tok.StartsWith("--host=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $ListenHost = $tok.Substring("--host=".Length)
                continue
            }
            if ($tok.StartsWith("--port=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $ListenPort = [int]$tok.Substring("--port=".Length)
                continue
            }
            if ($tok.StartsWith("--ui-port=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $UiPort = [int]$tok.Substring("--ui-port=".Length)
                continue
            }
            if ($tok.StartsWith("--web-port=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $UiPort = [int]$tok.Substring("--web-port=".Length)
                continue
            }
            if ($tok.StartsWith("--api-key=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $ApiKey = $tok.Substring("--api-key=".Length)
                $ExplicitApiKey = $true
                continue
            }
            $ClaudeArgs.Add($tok)
        }
    }
}

if ($ShowHelp) {
    Write-Host "Usage: .\start-agy.cmd [options] [--] [claude-or-codex-args...]"
    Write-Host ""
    Write-Host "Options:"
    Write-Host "  -Codex, --codex                  Launch Codex CLI instead of Claude Code"
    Write-Host "  -NoBypass, --no-bypass           Do not pass autonomous permission/sandbox bypass flags"
    Write-Host "  -BackgroundOnly, --services-only Start gateway + dashboard and sync configs only"
    Write-Host "  -StopServices, --stop            Stop background gateway and dashboard processes"
    Write-Host "  -GatewayModel <id>               Primary model (default: $GatewayModel)"
    Write-Host "  -FastModel <id>                  Fast/Haiku model (default: $FastModel)"
    Write-Host "  -ListenHost <host>               Gateway bind/connect host (default: $ListenHost)"
    Write-Host "  -ListenPort <port>               Gateway port (default: $ListenPort)"
    Write-Host "  -UiPort <port>                   Companion Vite dev port (default: $UiPort)"
    Write-Host "  -ApiKey <key>                    Bearer token / API key (default: local-agy)"
    Write-Host "  -InstallEnv / -UninstallEnv      Persist or clear Windows User profile env vars"
    Write-Host "  -h, --help                       Show this help message"
    Write-Host ""
    Write-Host "By default, Claude Code starts with --dangerously-skip-permissions and Codex starts with"
    Write-Host "--dangerously-bypass-approvals-and-sandbox. Remaining arguments pass through to the CLI."
    return
}

$RootDir = if (-not [string]::IsNullOrWhiteSpace($PSScriptRoot)) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
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
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_AUTO_MODE_SERVER",
    "CLAUDE_CODE_EXPERIMENTAL_CODE_MODE",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "NO_PROXY",
    "AGY_API_KEY",
    "AGY_GATEWAY_API_KEY"
)

if ($UninstallEnv) {
    if ($IsWin) {
        foreach ($k in $EnvKeys) {
            Remove-ItemProperty -Path "HKCU:\Environment" -Name $k -ErrorAction SilentlyContinue
        }
        Write-Host "[OK] Cleared persisted User-level gateway environment variables." -ForegroundColor Green
    } else {
        Write-Host "[INFO] On Linux/macOS, remove the sourced .agy-runtime/env.sh line from your shell rc if added." -ForegroundColor Yellow
    }
    return
}

function Stop-ProcessTree([int]$ProcId) {
    try {
        $proc = [System.Diagnostics.Process]::GetProcessById($ProcId)
        $killTree = $proc.GetType().GetMethod("Kill", [type[]]@([bool]))
        if ($null -ne $killTree) {
            $proc.Kill($true)
            return
        }
    } catch {}
    if ($IsWin) {
        Start-Process -FilePath "taskkill.exe" -ArgumentList @("/PID", "$ProcId", "/T", "/F") -WindowStyle Hidden -Wait -ErrorAction SilentlyContinue | Out-Null
    }
    Stop-Process -Id $ProcId -Force -ErrorAction SilentlyContinue
}

function Stop-TrackedProcess([string]$PidFile, [string]$Label) {
    if (Test-Path $PidFile) {
        $rawPid = (Get-Content $PidFile -ErrorAction SilentlyContinue | Select-Object -First 1)
        if ($rawPid -match '^\d+$') {
            $procId = [int]$rawPid
            $proc = Get-Process -Id $procId -ErrorAction SilentlyContinue
            if ($null -ne $proc) {
                Stop-ProcessTree -ProcId $procId
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

function Start-BackgroundProcess(
    [string]$Executable,
    [string[]]$ArgList,
    [string]$WorkDir,
    [string]$StdOutPath,
    [string]$StdErrPath
) {
    $filePath = $Executable
    $argLine = Join-Win32Args $ArgList
    if ($IsWin -and ($Executable.EndsWith(".cmd", [System.StringComparison]::OrdinalIgnoreCase) -or $Executable.EndsWith(".bat", [System.StringComparison]::OrdinalIgnoreCase))) {
        $filePath = if ($env:ComSpec) { $env:ComSpec } else { "cmd.exe" }
        $quotedExe = Format-Win32Arg $Executable
        $argLine = "/d /s /c `"$quotedExe $argLine`""
    }
    $startParams = @{
        FilePath               = $filePath
        WorkingDirectory       = $WorkDir
        RedirectStandardOutput = $StdOutPath
        RedirectStandardError  = $StdErrPath
        PassThru               = $true
    }
    if (-not [string]::IsNullOrWhiteSpace($argLine)) {
        $startParams["ArgumentList"] = $argLine
    }
    if ($IsWin) {
        $startParams["WindowStyle"] = "Hidden"
    }
    return (Start-Process @startParams)
}

function Invoke-LoopbackWebRequest([hashtable]$Params) {
    $callParams = @{}
    foreach ($kv in $Params.GetEnumerator()) {
        $callParams[$kv.Key] = $kv.Value
    }
    $callParams["UseBasicParsing"] = $true
    if ($PSVersionTable.PSVersion.Major -ge 6) {
        $callParams["NoProxy"] = $true
    }
    return (Invoke-WebRequest @callParams)
}

function Invoke-LoopbackRestMethod([hashtable]$Params) {
    $callParams = @{}
    foreach ($kv in $Params.GetEnumerator()) {
        $callParams[$kv.Key] = $kv.Value
    }
    if ($PSVersionTable.PSVersion.Major -ge 6) {
        $callParams["NoProxy"] = $true
    }
    return (Invoke-RestMethod @callParams)
}

function Test-HttpOk([string]$Url) {
    try {
        $resp = Invoke-LoopbackWebRequest @{ Uri = $Url; TimeoutSec = 2 }
        return ($resp.StatusCode -ge 200 -and $resp.StatusCode -lt 300)
    } catch {
        return $false
    }
}

function Test-SafePath([string]$Candidate) {
    if ([string]::IsNullOrWhiteSpace($Candidate)) {
        return $false
    }
    try {
        return [bool](Test-Path -LiteralPath $Candidate -ErrorAction SilentlyContinue)
    } catch {
        return $false
    }
}

function Resolve-BunExecutable {
    if ($IsWin) {
        $exeCmd = Get-Command bun.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $exeCmd -and (Test-SafePath $exeCmd.Source)) {
            return $exeCmd.Source
        }
        foreach ($c in (Get-Command bun -All -ErrorAction SilentlyContinue)) {
            if (-not [string]::IsNullOrWhiteSpace($c.Source)) {
                $shimDir = Split-Path -Parent $c.Source
                $nestedExe = Join-Path $shimDir "node_modules\bun\bin\bun.exe"
                if (Test-SafePath $nestedExe) {
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
            if (Test-SafePath $cand) {
                return $cand
            }
        }
        $cmdShim = Get-Command bun.cmd,bun.bat -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $cmdShim -and (Test-SafePath $cmdShim.Source)) {
            return $cmdShim.Source
        }
        return $null
    }

    $unixCmd = Get-Command bun -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $unixCmd -and (Test-SafePath $unixCmd.Source)) {
        return $unixCmd.Source
    }
    foreach ($cand in @((Join-Path $HOME ".bun/bin/bun"), "/opt/homebrew/bin/bun", "/usr/local/bin/bun")) {
        if (Test-SafePath $cand) {
            return $cand
        }
    }
    return $null
}

function Resolve-AgyExecutable {
    if ($IsWin) {
        $exeCmd = Get-Command agy.exe,agy.cmd -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $exeCmd -and (Test-SafePath $exeCmd.Source)) {
            return $exeCmd.Source
        }
    } else {
        $unixCmd = Get-Command agy -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $unixCmd -and (Test-SafePath $unixCmd.Source)) {
            return $unixCmd.Source
        }
    }
    $candidates = @(
        $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA "agy\bin\agy.exe" }),
        $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA "Programs\Antigravity\bin\agy.exe" }),
        (Join-Path $HOME ".local/bin/agy"),
        "/opt/homebrew/bin/agy",
        "/usr/local/bin/agy"
    )
    foreach ($c in $candidates) {
        if (Test-SafePath $c) {
            return $c
        }
    }
    return $null
}

function Resolve-NodeExecutable([string]$PreferredDir) {
    $nodeBin = if ($IsWin) { "node.exe" } else { "node" }
    if (-not [string]::IsNullOrWhiteSpace($PreferredDir)) {
        $localNode = Join-Path $PreferredDir $nodeBin
        if (Test-SafePath $localNode) {
            return $localNode
        }
    }
    $nodeCmd = Get-Command $nodeBin -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $nodeCmd -and (Test-SafePath $nodeCmd.Source)) {
        return $nodeCmd.Source
    }
    if ($IsWin) {
        $winNodeCandidates = @(
            $(if ($env:NVM_SYMLINK) { Join-Path $env:NVM_SYMLINK "node.exe" }),
            "C:\nvm4w\nodejs\node.exe",
            $(if ($env:ProgramFiles) { Join-Path $env:ProgramFiles "nodejs\node.exe" }),
            $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA "Programs\nodejs\node.exe" }),
            $(if ($env:VOLTA_HOME) { Join-Path $env:VOLTA_HOME "bin\node.exe" }),
            $(if ($env:FNM_MULTISHELL_PATH) { Join-Path $env:FNM_MULTISHELL_PATH "node.exe" })
        )
        foreach ($cand in $winNodeCandidates) {
            if (Test-SafePath $cand) {
                return $cand
            }
        }
    }
    return $null
}

function Resolve-ClaudeCommand {
    if ($IsWin) {
        $exeCmd = Get-Command claude.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $exeCmd -and (Test-SafePath $exeCmd.Source)) {
            return @{ Exe = $exeCmd.Source; PrefixArgs = @() }
        }
        foreach ($cand in @(
            (Join-Path $HOME ".local\bin\claude.exe"),
            $(if ($env:USERPROFILE) { Join-Path $env:USERPROFILE ".local\bin\claude.exe" }),
            $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA "Programs\claude\claude.exe" }),
            (Join-Path $HOME ".bun\bin\claude.exe"),
            $(if ($env:USERPROFILE) { Join-Path $env:USERPROFILE ".bun\bin\claude.exe" })
        )) {
            if (Test-SafePath $cand) {
                return @{ Exe = $cand; PrefixArgs = @() }
            }
        }
        $shimDirs = [System.Collections.Generic.List[string]]::new()
        foreach ($c in (Get-Command claude -All -ErrorAction SilentlyContinue)) {
            if (-not [string]::IsNullOrWhiteSpace($c.Source)) {
                $shimDirs.Add((Split-Path -Parent $c.Source))
            }
        }
        foreach ($extraDir in @(
            $(if ($env:APPDATA) { Join-Path $env:APPDATA "npm" }),
            $env:NVM_SYMLINK,
            "C:\nvm4w\nodejs"
        )) {
            if (-not [string]::IsNullOrWhiteSpace($extraDir)) {
                $shimDirs.Add($extraDir)
            }
        }
        foreach ($shimDir in $shimDirs) {
            $cliJs = Join-Path $shimDir "node_modules\@anthropic-ai\claude-code\cli.js"
            if (Test-SafePath $cliJs) {
                $nodeExe = Resolve-NodeExecutable -PreferredDir $shimDir
                if ($null -ne $nodeExe) {
                    return @{ Exe = $nodeExe; PrefixArgs = @($cliJs) }
                }
            }
        }
        $cmdShim = Get-Command claude.cmd,claude.bat -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $cmdShim -and (Test-SafePath $cmdShim.Source)) {
            return @{ Exe = $cmdShim.Source; PrefixArgs = @() }
        }
    }
    $fallback = Get-Command claude -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $fallback -or [string]::IsNullOrWhiteSpace($fallback.Source)) {
        throw "Claude Code CLI ('claude') was not found in PATH or ~/.local/bin."
    }
    return @{ Exe = $fallback.Source; PrefixArgs = @() }
}

function Resolve-CodexCommand {
    if ($IsWin) {
        $exeCmd = Get-Command codex.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $exeCmd -and (Test-SafePath $exeCmd.Source)) {
            return @{ Exe = $exeCmd.Source; PrefixArgs = @() }
        }
        foreach ($cand in @(
            (Join-Path $HOME ".bun\bin\codex.exe"),
            $(if ($env:USERPROFILE) { Join-Path $env:USERPROFILE ".bun\bin\codex.exe" }),
            $(if ($env:LOCALAPPDATA) { Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Links\codex.exe" }),
            (Join-Path $HOME ".cargo\bin\codex.exe"),
            (Join-Path $HOME ".local\bin\codex.exe")
        )) {
            if (Test-SafePath $cand) {
                return @{ Exe = $cand; PrefixArgs = @() }
            }
        }
        $shimDirs = [System.Collections.Generic.List[string]]::new()
        foreach ($c in (Get-Command codex -All -ErrorAction SilentlyContinue)) {
            if (-not [string]::IsNullOrWhiteSpace($c.Source)) {
                $shimDirs.Add((Split-Path -Parent $c.Source))
            }
        }
        foreach ($extraDir in @(
            $(if ($env:APPDATA) { Join-Path $env:APPDATA "npm" }),
            $env:NVM_SYMLINK,
            "C:\nvm4w\nodejs"
        )) {
            if (-not [string]::IsNullOrWhiteSpace($extraDir)) {
                $shimDirs.Add($extraDir)
            }
        }
        foreach ($shimDir in $shimDirs) {
            $codexJs = Join-Path $shimDir "node_modules\@openai\codex\bin\codex.js"
            if (Test-SafePath $codexJs) {
                $nodeExe = Resolve-NodeExecutable -PreferredDir $shimDir
                if ($null -ne $nodeExe) {
                    return @{ Exe = $nodeExe; PrefixArgs = @($codexJs) }
                }
            }
        }
        $cmdShim = Get-Command codex.cmd,codex.bat -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($null -ne $cmdShim -and (Test-SafePath $cmdShim.Source)) {
            return @{ Exe = $cmdShim.Source; PrefixArgs = @() }
        }
    }
    $fallback = Get-Command codex -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $fallback -or [string]::IsNullOrWhiteSpace($fallback.Source)) {
        throw "Codex CLI ('codex') was not found in PATH."
    }
    return @{ Exe = $fallback.Source; PrefixArgs = @() }
}

function Get-WindowsSystemProxyUrl {
    if (-not $IsWin) {
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

# Snapshot caller shell environment before mutating any process env vars
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
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_AUTO_MODE_SERVER",
    "AGY_API_KEY",
    "AGY_GATEWAY_API_KEY",
    "VITE_GATEWAY_PROXY",
    "NO_PROXY",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "PATH"
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

$cliExitCode = 0
$keepEnvInProcess = $false
try {
    if ($IsWin -and -not [string]::IsNullOrWhiteSpace($env:PATH) -and $env:PATH.Contains("%")) {
        $env:PATH = [System.Environment]::ExpandEnvironmentVariables($env:PATH)
    }

    # Ensure loopback checks never route through a local HTTP proxy
    $env:NO_PROXY = "127.0.0.1,localhost,::1"
    Remove-Item Env:\HTTP_PROXY -ErrorAction SilentlyContinue
    Remove-Item Env:\HTTPS_PROXY -ErrorAction SilentlyContinue

    # ============================================================================
    # 1. Ensure liftgate is running
    # ============================================================================
    if (Test-HttpOk "$GatewayBaseUrl/healthz") {
        Write-Host "[1/4] Gateway already running at $GatewayBaseUrl" -ForegroundColor Cyan
    } else {
        $binName = if ($IsWin) { "liftgate.exe" } else { "liftgate" }
        $gwBin = Join-Path $RootDir "target\release\$binName"
        if (-not (Test-Path $gwBin)) {
            $distIndex = Join-Path $RootDir "web\dist\index.html"
            if (-not (Test-Path $distIndex)) {
                $buildBun = Resolve-BunExecutable
                if ($null -ne $buildBun) {
                    Write-Host "[1/4] Building embedded web bundle (web/dist) ..." -ForegroundColor Cyan
                    $webBuildDir = Join-Path $RootDir "web"
                    & $buildBun install --cwd $webBuildDir --frozen-lockfile
                    if ($LASTEXITCODE -eq 0) {
                        & $buildBun run --cwd $webBuildDir build
                    }
                }
            }
            Write-Host "[1/4] Building release liftgate ..." -ForegroundColor Cyan
            Push-Location $RootDir
            try {
                & cargo build --release --locked -p liftgate
                if ($LASTEXITCODE -ne 0) {
                    throw "cargo build failed with exit code $LASTEXITCODE"
                }
            } finally {
                Pop-Location
            }
        }

        Write-Host "[1/4] Starting liftgate on $GatewayBaseUrl (default model: $GatewayModel) ..." -ForegroundColor Cyan
        $gwArgs = [System.Collections.Generic.List[string]]::new()
        $gwArgs.Add("--listen")
        $gwArgs.Add("${ListenHost}:${ListenPort}")
        $gwArgs.Add("--default-model")
        $gwArgs.Add($GatewayModel)
        $gwArgs.Add("--acp-mode")
        $gwArgs.Add("auto")
        if ($ExplicitApiKey) {
            $gwArgs.Add("--api-key")
            $gwArgs.Add($ApiKey)
        }
        $gwProc = Start-BackgroundProcess `
            -Executable $gwBin `
            -ArgList $gwArgs.ToArray() `
            -WorkDir $RootDir `
            -StdOutPath $GatewayLogOut `
            -StdErrPath $GatewayLogErr
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
            Write-Host "[2/4] Bun not found in PATH; embedded UI is live at $GatewayBaseUrl/" -ForegroundColor Yellow
        } else {
            try {
                $webDir = Join-Path $RootDir "web"
                $nodeModulesDir = Join-Path $webDir "node_modules"
                if (-not (Test-Path $nodeModulesDir)) {
                    Write-Host "[2/4] Installing frontend dependencies via Bun ..." -ForegroundColor Cyan
                    & $bunExe install --cwd $webDir
                }
                Write-Host "[2/4] Starting companion frontend on $WebBaseUrl/ ..." -ForegroundColor Cyan
                $env:VITE_GATEWAY_PROXY = $GatewayBaseUrl
                $webProc = Start-BackgroundProcess `
                    -Executable $bunExe `
                    -ArgList @("run", "dev", "--host", $ListenHost, "--port", "$UiPort") `
                    -WorkDir $webDir `
                    -StdOutPath $WebLogOut `
                    -StdErrPath $WebLogErr
                Set-Content -Path $WebPidFile -Value "$($webProc.Id)" -NoNewline

                $webReady = $false
                for ($i = 0; $i -lt 30; $i++) {
                    Start-Sleep -Milliseconds 300
                    if ($webProc.HasExited) {
                        break
                    }
                    if (Test-HttpOk "$WebBaseUrl/") {
                        $webReady = $true
                        break
                    }
                }
                if ($webReady) {
                    Write-Host "      Frontend online (PID $($webProc.Id)) -> $WebBaseUrl/" -ForegroundColor Green
                } else {
                    Write-Host "      Frontend dev server unavailable or warming up; embedded UI ready at $GatewayBaseUrl/" -ForegroundColor Yellow
                }
            } catch {
                Write-Host "      Warning: could not start dev frontend ($($_.Exception.Message)); embedded UI ready at $GatewayBaseUrl/" -ForegroundColor Yellow
            }
        }
    }

    # ============================================================================
    # 3. Dynamic Model Discovery (/v1/models) -> Claude Code & Codex CLI
    # ============================================================================
    Write-Host "[3/4] Discovering live models from $GatewayBaseUrl/v1/models ..." -ForegroundColor Cyan
    $modelsResp = Invoke-LoopbackRestMethod @{
        Uri        = "$GatewayBaseUrl/v1/models"
        Headers    = @{ Authorization = "Bearer $ApiKey" }
        TimeoutSec = 15
    }

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
            Remove-Item Env:\HTTP_PROXY -ErrorAction SilentlyContinue
            Remove-Item Env:\HTTPS_PROXY -ErrorAction SilentlyContinue
            $modelsResp = Invoke-LoopbackRestMethod @{
                Uri        = "$GatewayBaseUrl/v1/models"
                Headers    = @{ Authorization = "Bearer $ApiKey" }
                TimeoutSec = 15
            }
        }
    }

    $pickerOptions = [System.Collections.Generic.List[object]]::new()
    $modelSettings = [ordered]@{}
    $compactLimits = @{}
    $midDot = [char]0x00B7

    function Add-ModelCompactWindow([string]$ModelId, [long]$Limit) {
        if ($modelSettings.Contains($ModelId)) {
            $Limit = [math]::Min($Limit, [long]$modelSettings[$ModelId].autoCompactWindow)
        }
        $modelSettings[$ModelId] = [ordered]@{ autoCompactWindow = $Limit }
    }

    foreach ($m in $modelsResp.data) {
        $id = [string]$m.id
        $display = if (-not [string]::IsNullOrWhiteSpace($m.display_name)) { [string]$m.display_name } else { $id }
        $ctxK = if ($null -ne $m.context_window -and [long]$m.context_window -gt 0) {
            "$([math]::Round([double]$m.context_window / 1000))k ctx"
        } else {
            "unknown ctx"
        }
        $lower = $id.ToLowerInvariant()
        $behavesAs = if ($lower.StartsWith("gpt-oss")) {
            "claude-haiku-4-5-20251001"
        } elseif ($lower.StartsWith("gemini-")) {
            if ($lower.Contains("flash-low")) { "claude-haiku-5-5" }
            elseif ($lower.Contains("sonnet") -or $lower.EndsWith("-medium")) { "claude-sonnet-5-5" }
            else { "claude-opus-5-5" }
        } elseif ($lower.Contains("sonnet") -or $lower.EndsWith("-medium")) {
            "claude-sonnet-4-6"
        } else {
            "claude-opus-4-6"
        }
        # Older gateways do not expose an auto-compact limit. Stay conservative
        # rather than inferring a safe threshold from a possibly inflated window.
        $limit = if ($null -ne $m.auto_compact_token_limit -and [long]$m.auto_compact_token_limit -ge 100000) {
            [math]::Min([long]$m.auto_compact_token_limit, 1000000)
        } else { 110000 }
        $compactLimits[$id] = $limit
        Add-ModelCompactWindow $id $limit
        Add-ModelCompactWindow $behavesAs $limit

        $pickerOptions.Add([ordered]@{
            model       = $id
            label       = $display
            description = "$ctxK $midDot Antigravity ACP ($id)"
            behavesAs   = $behavesAs
        })
    }

    $detectedProxy = if (-not [string]::IsNullOrWhiteSpace($SavedProcessEnv["HTTPS_PROXY"])) {
        $SavedProcessEnv["HTTPS_PROXY"]
    } elseif (-not [string]::IsNullOrWhiteSpace($SavedProcessEnv["HTTP_PROXY"])) {
        $SavedProcessEnv["HTTP_PROXY"]
    } else {
        Get-WindowsSystemProxyUrl
    }

    if (-not $ExplicitFastModel) {
        $flashCandidates = @(
            $modelsResp.data |
                ForEach-Object { [string]$_.id } |
                Where-Object { $_ -match '^gemini-(?!3\.8-flash).*flash.*-low$' } |
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
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC  = "1"
        CLAUDE_CODE_AUTO_MODE_SERVER              = "0"
        CLAUDE_CODE_EXPERIMENTAL_CODE_MODE        = "1"
        NO_PROXY                                  = "127.0.0.1,localhost,::1"
    }
    if (-not [string]::IsNullOrWhiteSpace($detectedProxy)) {
        $envMap["HTTP_PROXY"]  = $detectedProxy
        $envMap["HTTPS_PROXY"] = $detectedProxy
    }

    $settingsObj = [ordered]@{
        model = $GatewayModel
    }
    if (-not $NoBypass) {
        $settingsObj["skipDangerousModePermissionPrompt"] = $true
        $settingsObj["permissions"] = [ordered]@{
            defaultMode = "bypassPermissions"
        }
    }
    $settingsObj["env"] = $envMap
    $settingsObj["modelOverrides"] = [ordered]@{
        "claude-fable-5-2"          = $GatewayModel
        "claude-opus-5-5"           = $GatewayModel
        "claude-sonnet-5-5"         = $GatewayModel
        "claude-haiku-5-5"          = $FastModel
        "claude-opus-4-6"           = $GatewayModel
        "claude-sonnet-4-6"         = $GatewayModel
        "claude-haiku-4-5-20251001" = $FastModel
    }
    foreach ($entry in $settingsObj["modelOverrides"].GetEnumerator()) {
        $limit = if ($compactLimits.ContainsKey([string]$entry.Value)) { $compactLimits[[string]$entry.Value] } else { 110000 }
        Add-ModelCompactWindow ([string]$entry.Key) $limit
    }
    $settingsObj["autoCompactWindow"] = 110000
    $settingsObj["modelSettings"] = $modelSettings
    $settingsObj["modelPicker"] = [ordered]@{
        replaceBuiltInOptions = $true
        options               = $pickerOptions
    }

    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    $settingsJson = $settingsObj | ConvertTo-Json -Depth 10
    [System.IO.File]::WriteAllText($SettingsPath, $settingsJson, $utf8NoBom)
    New-Item -ItemType Directory -Force -Path $LocalClaudeDir | Out-Null
    [System.IO.File]::WriteAllText($LocalSettingsPath, $settingsJson, $utf8NoBom)

    $codexCatalogPath = Join-Path $RuntimeDir "codex-model-catalog.json"
    $codexCatalogSlash = $codexCatalogPath.Replace('\', '/')
    $encodedDefaultModel = [uri]::EscapeDataString($GatewayModel)
    Invoke-LoopbackWebRequest @{
        Uri        = "$GatewayBaseUrl/v1/codex-models?default_model=$encodedDefaultModel"
        Headers    = @{ Authorization = "Bearer $ApiKey" }
        TimeoutSec = 15
        OutFile    = $codexCatalogPath
    } | Out-Null

    $codexConfigPath = Join-Path $RuntimeDir "codex-config.toml"
    $codexBypassLines = if (-not $NoBypass) {
        "approval_policy = `"never`"`nsandbox_mode = `"danger-full-access`"`n"
    } else {
        ""
    }
    $codexToml = @"
model = "$GatewayModel"
model_provider = "agy"
model_catalog_json = "$codexCatalogSlash"
${codexBypassLines}web_search = "live"
suppress_unstable_features_warning = true

[features]
code_mode = true

[model_providers.agy]
name = "Liftgate"
base_url = "$GatewayBaseUrl/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"
"@
    [System.IO.File]::WriteAllText($codexConfigPath, $codexToml, $utf8NoBom)

    Write-Host "      Synced $($pickerOptions.Count) live models into Claude modelPicker & Codex model_catalog_json (.agy-runtime/codex-model-catalog.json)" -ForegroundColor Green

    # ============================================================================
    # 4. Configure Scoped Process & Optional Persisted User Environment Variables
    # ============================================================================
    Remove-Item Env:\ANTHROPIC_API_KEY -ErrorAction SilentlyContinue
    Remove-Item Env:\AGY_API_KEY -ErrorAction SilentlyContinue
    Remove-Item Env:\CLAUDE_CODE_MAX_CONTEXT_TOKENS -ErrorAction SilentlyContinue
    Remove-Item Env:\CLAUDE_CODE_AUTO_COMPACT_WINDOW -ErrorAction SilentlyContinue
    if ($InstallEnv -and $IsWin) {
        Remove-ItemProperty -Path "HKCU:\Environment" -Name "ANTHROPIC_API_KEY" -ErrorAction SilentlyContinue
        Remove-ItemProperty -Path "HKCU:\Environment" -Name "AGY_API_KEY" -ErrorAction SilentlyContinue
        Remove-ItemProperty -Path "HKCU:\Environment" -Name "CLAUDE_CODE_MAX_CONTEXT_TOKENS" -ErrorAction SilentlyContinue
        Remove-ItemProperty -Path "HKCU:\Environment" -Name "CLAUDE_CODE_AUTO_COMPACT_WINDOW" -ErrorAction SilentlyContinue
        $curOpenAiKey = (Get-ItemProperty -Path "HKCU:\Environment" -ErrorAction SilentlyContinue).OPENAI_API_KEY
        if ($curOpenAiKey -eq $ApiKey) {
            Remove-ItemProperty -Path "HKCU:\Environment" -Name "OPENAI_API_KEY" -ErrorAction SilentlyContinue
            Remove-ItemProperty -Path "HKCU:\Environment" -Name "OPENAI_BASE_URL" -ErrorAction SilentlyContinue
        }
    }

    # OPENAI_BASE_URL and OPENAI_API_KEY are intentionally NOT set globally
    # so plain `codex` outside `-Codex` preserves your default Codex setup.
    $fullEnvMap = [ordered]@{
        ANTHROPIC_BASE_URL                        = $GatewayBaseUrl
        ANTHROPIC_AUTH_TOKEN                      = $ApiKey
        ANTHROPIC_MODEL                           = $GatewayModel
        ANTHROPIC_DEFAULT_OPUS_MODEL              = $GatewayModel
        ANTHROPIC_DEFAULT_SONNET_MODEL            = $GatewayModel
        ANTHROPIC_DEFAULT_HAIKU_MODEL             = $FastModel
        ANTHROPIC_SMALL_FAST_MODEL                = $FastModel
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC  = "1"
        CLAUDE_CODE_AUTO_MODE_SERVER              = "0"
        CLAUDE_CODE_EXPERIMENTAL_CODE_MODE        = "1"
        AGY_GATEWAY_API_KEY                       = $ApiKey
        NO_PROXY                                  = "127.0.0.1,localhost,::1"
    }
    if (-not [string]::IsNullOrWhiteSpace($detectedProxy)) {
        $env:HTTP_PROXY  = $detectedProxy
        $env:HTTPS_PROXY = $detectedProxy
    }

    foreach ($entry in $fullEnvMap.GetEnumerator()) {
        Set-Item -Path "Env:\$($entry.Key)" -Value $entry.Value
        if ($InstallEnv -and $IsWin) {
            Set-ItemProperty -Path "HKCU:\Environment" -Name $entry.Key -Value $entry.Value
        }
    }

    $shExportLines = @(
        "unset ANTHROPIC_API_KEY",
        "unset AGY_API_KEY",
        "unset CLAUDE_CODE_MAX_CONTEXT_TOKENS",
        "unset CLAUDE_CODE_AUTO_COMPACT_WINDOW"
    ) + @(foreach ($entry in $fullEnvMap.GetEnumerator()) {
        "export $($entry.Key)=`"$($entry.Value)`""
    })
    [System.IO.File]::WriteAllLines((Join-Path $RuntimeDir "env.sh"), $shExportLines, $utf8NoBom)

    if ($InstallEnv -and $IsWin) {
        $keepEnvInProcess = $true
        Write-Host "[4/4] Persisted gateway environment variables to Windows User profile (isolated AGY_GATEWAY_API_KEY for Codex)." -ForegroundColor Green
    } else {
        Write-Host "[4/4] Scoped environment ready (restores original shell env automatically upon CLI exit)." -ForegroundColor Green
    }

    $bypassLabel = if ($NoBypass) { "standard permissions (-NoBypass)" } else { "full autonomous bypass" }
    Write-Host ""
    Write-Host "  Gateway API & Embedded UI : $GatewayBaseUrl/" -ForegroundColor White
    Write-Host "  Companion Dev Frontend    : $WebBaseUrl/" -ForegroundColor White
    Write-Host "  Models Discovery Endpoint : $GatewayBaseUrl/v1/models ($($pickerOptions.Count) models)" -ForegroundColor White
    Write-Host "  Active Primary Model      : $GatewayModel (Fast/Subagent: $FastModel)" -ForegroundColor White
    Write-Host "  Execution Mode            : $bypassLabel" -ForegroundColor White
    Write-Host ""

    if ($BackgroundOnly) {
        return
    }

    if ($LaunchCodex) {
        $hasExplicitCodexModel = $false
        $hasExplicitCodexApproval = $false
        foreach ($ca in $ClaudeArgs) {
            if (
                $ca -eq "-m" -or
                $ca -eq "--model" -or
                $ca.StartsWith("-m=", [System.StringComparison]::OrdinalIgnoreCase) -or
                $ca.StartsWith("--model=", [System.StringComparison]::OrdinalIgnoreCase)
            ) {
                $hasExplicitCodexModel = $true
            }
            if (
                $ca -eq "--dangerously-bypass-approvals-and-sandbox" -or
                $ca -eq "--yolo" -or
                $ca -eq "-s" -or $ca -eq "--sandbox" -or
                $ca.StartsWith("-s=", [System.StringComparison]::OrdinalIgnoreCase) -or
                $ca.StartsWith("--sandbox=", [System.StringComparison]::OrdinalIgnoreCase) -or
                $ca -eq "-a" -or $ca -eq "--ask-for-approval" -or
                $ca.StartsWith("-a=", [System.StringComparison]::OrdinalIgnoreCase) -or
                $ca.StartsWith("--ask-for-approval=", [System.StringComparison]::OrdinalIgnoreCase) -or
                $ca -eq "--approve-for-me" -or $ca -eq "--full-auto"
            ) {
                $hasExplicitCodexApproval = $true
            }
        }

        $codexCmd = Resolve-CodexCommand
        $codexLaunchArgs = [System.Collections.Generic.List[string]]::new()
        foreach ($pa in $codexCmd.PrefixArgs) {
            $codexLaunchArgs.Add([string]$pa)
        }
        if (-not $NoBypass -and -not $hasExplicitCodexApproval) {
            $codexLaunchArgs.Add("--dangerously-bypass-approvals-and-sandbox")
        }
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("model_provider='agy'")
        if (-not $hasExplicitCodexModel) {
            $codexLaunchArgs.Add("-c")
            $codexLaunchArgs.Add("model='$GatewayModel'")
        }
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("model_catalog_json='$codexCatalogSlash'")
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("web_search='live'")
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("model_providers.agy.name='Liftgate'")
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("model_providers.agy.base_url='$GatewayBaseUrl/v1'")
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("model_providers.agy.env_key='AGY_GATEWAY_API_KEY'")
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("model_providers.agy.wire_api='responses'")
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("suppress_unstable_features_warning=true")
        $codexLaunchArgs.Add("-c")
        $codexLaunchArgs.Add("features.code_mode=true")
        foreach ($ca in $ClaudeArgs) {
            $codexLaunchArgs.Add($ca)
        }

        # Invoke directly in top-level scope (never via `$x = Function`) so the
        # child TUI inherits the real console stdin/stdout handles (isTTY = true).
        $global:LASTEXITCODE = 0
        & $codexCmd.Exe @codexLaunchArgs
        $cliExitCode = if ($null -ne $LASTEXITCODE) { [int]$LASTEXITCODE } else { 0 }
    } else {
        $hasExplicitModel = $false
        $hasExplicitClaudePerm = $false
        foreach ($ca in $ClaudeArgs) {
            if ($ca -eq "--model" -or $ca.StartsWith("--model=", [System.StringComparison]::OrdinalIgnoreCase)) {
                $hasExplicitModel = $true
            }
            if (
                $ca -eq "--dangerously-skip-permissions" -or
                $ca -eq "--allow-dangerously-skip-permissions" -or
                $ca -eq "--permission-mode" -or
                $ca.StartsWith("--permission-mode=", [System.StringComparison]::OrdinalIgnoreCase) -or
                $ca -eq "--safe-mode" -or $ca -eq "--restricted"
            ) {
                $hasExplicitClaudePerm = $true
            }
        }

        $claudeCmd = Resolve-ClaudeCommand
        $claudeLaunchArgs = [System.Collections.Generic.List[string]]::new()
        foreach ($pa in $claudeCmd.PrefixArgs) {
            $claudeLaunchArgs.Add([string]$pa)
        }
        if (-not $NoBypass -and -not $hasExplicitClaudePerm) {
            $claudeLaunchArgs.Add("--dangerously-skip-permissions")
        }
        $claudeLaunchArgs.Add("--setting-sources")
        $claudeLaunchArgs.Add("project,local")
        $claudeLaunchArgs.Add("--settings")
        $claudeLaunchArgs.Add($SettingsPath)
        if (-not $hasExplicitModel) {
            $claudeLaunchArgs.Add("--model")
            $claudeLaunchArgs.Add($GatewayModel)
        }
        foreach ($ca in $ClaudeArgs) {
            $claudeLaunchArgs.Add($ca)
        }

        # Invoke directly in top-level scope (never via `$x = Function`) so the
        # child TUI inherits the real console stdin/stdout handles (isTTY = true).
        $global:LASTEXITCODE = 0
        & $claudeCmd.Exe @claudeLaunchArgs
        $cliExitCode = if ($null -ne $LASTEXITCODE) { [int]$LASTEXITCODE } else { 0 }
    }
} finally {
    if (-not $keepEnvInProcess) {
        Restore-CallerProcessEnv
    }
}
exit $cliExitCode
