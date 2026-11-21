<#
.SYNOPSIS
  Liftgate CLI (`lg` + `liftgate`) installer for Windows (PowerShell).

.DESCRIPTION
  Downloads the prebuilt archive matching your Windows CPU (`x64`, `ARM64`, or
  `x86`) from GitHub Releases, verifies SHA-256 fail-closed against SHA256SUMS,
  smoke-tests `liftgate.exe` and `lg.exe`, installs them atomically, and adds
  the install directory to your User PATH.

  One-liner:
    irm https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.ps1 | iex

  Environment overrides:
    LIFTGATE_VERSION         pin a release tag (e.g. v0.2.0); default: latest
    LIFTGATE_BIN_DIR         install dir; default: existing install dir or %LOCALAPPDATA%\Programs\liftgate-cli
    LIFTGATE_NO_MODIFY_PATH  set to 1 to skip modifying User PATH
    LIFTGATE_TARGET          override Rust target triple (advanced)
    LIFTGATE_REPO            GitHub owner/repo; default: acking-you/liftgate
    LIFTGATE_BASE_URL        release host override; default: https://github.com/$LIFTGATE_REPO
#>

& {
  param(
    [string]$Version = '',
    [string]$BinDir = '',
    [switch]$NoModifyPath,
    [switch]$Help
  )

  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'
  try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

  if ($Help) {
    Write-Host @'
Liftgate CLI installer (`lg.exe` + `liftgate.exe`)

Usage:
  irm https://raw.githubusercontent.com/acking-you/liftgate/main/scripts/install.ps1 | iex
  .\scripts\install.ps1 [-Version <TAG>] [-BinDir <DIR>] [-NoModifyPath]
'@
    return
  }

  function Write-Info([string]$Message) { Write-Host "==> $Message" -ForegroundColor Cyan }
  function Write-WarnMsg([string]$Message) { Write-Host "warning: $Message" -ForegroundColor Yellow }

  function Normalize-Dir([string]$DirPath) {
    $full = [IO.Path]::GetFullPath($DirPath).TrimEnd('\', '/')
    if ($full -match ':$') { return "$full\" }
    return $full
  }

  $repo = if ($env:LIFTGATE_REPO) { $env:LIFTGATE_REPO } else { 'acking-you/liftgate' }
  if ($repo -notmatch '^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$') {
    throw "invalid LIFTGATE_REPO: $repo"
  }

  $baseUrl = if ($env:LIFTGATE_BASE_URL) { $env:LIFTGATE_BASE_URL.TrimEnd('/') } else { "https://github.com/$repo" }
  if ($baseUrl -notmatch '^(https://|http://127\.0\.0\.1:|http://localhost:|http://\[::1\]:)') {
    throw "LIFTGATE_BASE_URL must use https:// (got: $baseUrl)"
  }

  $rawVersion = if ($Version) { $Version } elseif ($env:LIFTGATE_VERSION) { $env:LIFTGATE_VERSION } else { '' }
  $tag = ''
  if ($rawVersion) {
    if ($rawVersion -like '*-ui') {
      $suggested = $rawVersion.Substring(0, $rawVersion.Length - 3)
      throw "'$rawVersion' is not a CLI release tag; omit '-ui' (e.g. '$suggested')"
    }
    $tag = if ($rawVersion -match '^[vV](.+)$') { "v$($Matches[1])" } else { "v$rawVersion" }
    if ($tag -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$') {
      throw "invalid version tag '$rawVersion' (expected e.g. v0.2.0)"
    }
  }

  $skipModifyPath = $NoModifyPath.IsPresent -or ($env:LIFTGATE_NO_MODIFY_PATH -eq '1')
  $explicitBinDir = if ($BinDir) { $BinDir } elseif ($env:LIFTGATE_BIN_DIR) { $env:LIFTGATE_BIN_DIR } else { '' }

  # --- detect CPU -> Rust target ---------------------------------------------
  $osArch = try { [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() } catch { '' }
  $envArch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  $archKey = if ($osArch) { $osArch } else { "$envArch" }
  $hostTarget = switch -Regex ($archKey) {
    '^(Arm64|ARM64)$' { 'aarch64-pc-windows-msvc'; break }
    '^(X64|AMD64)$'   { 'x86_64-pc-windows-msvc'; break }
    '^(X86)$'         { 'i686-pc-windows-msvc'; break }
    default           { throw "unsupported Windows CPU architecture: $archKey" }
  }

  if ($env:LIFTGATE_TARGET) {
    if ($env:LIFTGATE_TARGET -notmatch '^[A-Za-z0-9_-]+$') {
      throw "invalid LIFTGATE_TARGET: $($env:LIFTGATE_TARGET)"
    }
    $target = $env:LIFTGATE_TARGET
  }
  else {
    $target = $hostTarget
  }

  $archiveName = "liftgate-$target.zip"
  $releaseLabel = if ($tag) { $tag } else { 'latest' }
  $downloadBase = if ($tag) { "$baseUrl/releases/download/$tag" } else { "$baseUrl/releases/latest/download" }
  $archiveUrl = "$downloadBase/$archiveName"
  $checksumsUrl = "$downloadBase/SHA256SUMS"

  # --- inspect existing standalone CLI install -------------------------------
  $existingExe = $null
  if ($explicitBinDir) {
    $targetDir = Normalize-Dir $explicitBinDir
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
    $candExe = Join-Path $targetDir 'liftgate.exe'
    if (Test-Path -LiteralPath $candExe -PathType Leaf) {
      $existingExe = $candExe
    }
  }
  else {
    foreach ($cmdName in @('liftgate.exe', 'lg.exe')) {
      $cmd = Get-Command $cmdName -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
      if ($cmd -and $cmd.Source -and (Test-Path -LiteralPath $cmd.Source)) {
        $candDir = Split-Path -Parent $cmd.Source
        # Skip Inno Setup installer directories so packaged uninstallation never
        # removes standalone CLI binaries or vice versa.
        if (Test-Path -LiteralPath (Join-Path $candDir 'unins000.exe')) {
          continue
        }
        $verOut = try { & $cmd.Source --version 2>$null | Select-Object -First 1 } catch { $null }
        if ($verOut -match '^(liftgate|lg)\s+') {
          $existingExe = $cmd.Source
          break
        }
      }
    }

    $targetDir = Normalize-Dir $(
      if ($existingExe) { Split-Path -Parent $existingExe }
      else {
        $localAppData = if ($env:LOCALAPPDATA) { $env:LOCALAPPDATA } else { Join-Path $HOME 'AppData\Local' }
        Join-Path $localAppData 'Programs\liftgate-cli'
      }
    )
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
    if ((-not $existingExe) -and (Test-Path -LiteralPath (Join-Path $targetDir 'liftgate.exe') -PathType Leaf)) {
      $existingExe = Join-Path $targetDir 'liftgate.exe'
    }
  }

  if ($existingExe) {
    $curLine = try { & $existingExe --version 2>$null | Select-Object -First 1 } catch { $null }
    $curVer = if ($curLine) { ($curLine -split '\s+')[-1] } else { '(unknown)' }
    Write-Info "found Liftgate $curVer at $existingExe - updating ($releaseLabel / $target)"
  }
  else {
    Write-Info "installing Liftgate CLI ($releaseLabel / $target)"
  }

  # --- download & fail-closed SHA-256 verification ---------------------------
  $headers = @{ 'User-Agent' = 'liftgate-install' }
  $tmpDir = Join-Path ([IO.Path]::GetTempPath()) ("liftgate-install-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

  try {
    $zipPath = Join-Path $tmpDir $archiveName
    $sumsPath = Join-Path $tmpDir 'SHA256SUMS'

    Write-Info "downloading $archiveUrl"
    Invoke-WebRequest -Headers $headers -Uri $archiveUrl -OutFile $zipPath

    Write-Info 'verifying SHA-256 checksum against SHA256SUMS'
    Invoke-WebRequest -Headers $headers -Uri $checksumsUrl -OutFile $sumsPath

    $escapedName = [Regex]::Escape($archiveName)
    $expectedSha = $null
    foreach ($line in (Get-Content -LiteralPath $sumsPath)) {
      if ($line -match "^\s*([0-9a-fA-F]{64})\s+\*?$escapedName\s*$") {
        $expectedSha = $Matches[1].ToLowerInvariant()
        break
      }
    }
    if (-not $expectedSha) {
      throw "SHA256SUMS did not contain a valid SHA-256 entry for $archiveName"
    }

    $actualSha = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha -ne $expectedSha) {
      throw "SHA-256 mismatch for ${archiveName}: expected $expectedSha, got $actualSha"
    }

    # --- unpack & smoke-test CLI binaries (`liftgate.exe` + `lg.exe`) --------
    $extractDir = Join-Path $tmpDir 'extract'
    Expand-Archive -LiteralPath $zipPath -DestinationPath $extractDir -Force

    $pkgDir = Join-Path $extractDir "liftgate-$target"
    $srcMain = Join-Path $pkgDir 'liftgate.exe'
    $srcAlias = Join-Path $pkgDir 'lg.exe'
    if (-not (Test-Path -LiteralPath $srcMain -PathType Leaf)) {
      throw "archive $archiveName is missing 'liftgate.exe'"
    }
    if (-not (Test-Path -LiteralPath $srcAlias -PathType Leaf)) {
      throw "archive $archiveName is missing 'lg.exe'"
    }

    if (($env:LIFTGATE_SKIP_SMOKE_TEST -ne '1') -and ($target -eq $hostTarget)) {
      $null = & $srcMain --version
      if ($LASTEXITCODE -ne 0) { throw "downloaded 'liftgate.exe' failed smoke test (--version)" }
      $null = & $srcAlias --version
      if ($LASTEXITCODE -ne 0) { throw "downloaded 'lg.exe' failed smoke test (--version)" }
    }

    # --- stage & atomically swap in $targetDir (safe while daemon runs) ------
    $runId = [Guid]::NewGuid().ToString('N').Substring(0, 8)
    $stageMain = Join-Path $targetDir ".liftgate.$runId.new.exe"
    $stageAlias = Join-Path $targetDir ".lg.$runId.new.exe"
    $destMain = Join-Path $targetDir 'liftgate.exe'
    $destAlias = Join-Path $targetDir 'lg.exe'
    $oldMain = Join-Path $targetDir ".liftgate.$runId.old.exe"
    $oldAlias = Join-Path $targetDir ".lg.$runId.old.exe"

    try {
      Copy-Item -LiteralPath $srcMain -Destination $stageMain -Force
      Copy-Item -LiteralPath $srcAlias -Destination $stageAlias -Force

      # Clean up stale retired binaries from previous updates when unlocked.
      Get-ChildItem -LiteralPath $targetDir -Filter '.*.old.exe' -Force -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }

      $movedMain = $false
      $movedAlias = $false
      try {
        if (Test-Path -LiteralPath $destMain) {
          Move-Item -LiteralPath $destMain -Destination $oldMain -Force
          $movedMain = $true
        }
        Move-Item -LiteralPath $stageMain -Destination $destMain -Force

        if (Test-Path -LiteralPath $destAlias) {
          Move-Item -LiteralPath $destAlias -Destination $oldAlias -Force
          $movedAlias = $true
        }
        Move-Item -LiteralPath $stageAlias -Destination $destAlias -Force
      }
      catch {
        if ($movedMain -and (Test-Path -LiteralPath $oldMain)) {
          Move-Item -LiteralPath $oldMain -Destination $destMain -Force -ErrorAction SilentlyContinue
        }
        if ($movedAlias -and (Test-Path -LiteralPath $oldAlias)) {
          Move-Item -LiteralPath $oldAlias -Destination $destAlias -Force -ErrorAction SilentlyContinue
        }
        throw
      }
      finally {
        if (Test-Path -LiteralPath $oldMain) {
          Remove-Item -LiteralPath $oldMain -Force -ErrorAction SilentlyContinue
        }
        if (Test-Path -LiteralPath $oldAlias) {
          Remove-Item -LiteralPath $oldAlias -Force -ErrorAction SilentlyContinue
        }
      }
    }
    finally {
      if (Test-Path -LiteralPath $stageMain) {
        Remove-Item -LiteralPath $stageMain -Force -ErrorAction SilentlyContinue
      }
      if (Test-Path -LiteralPath $stageAlias) {
        Remove-Item -LiteralPath $stageAlias -Force -ErrorAction SilentlyContinue
      }
    }

    $installedLine = try { & $destMain --version 2>$null | Select-Object -First 1 } catch { $null }
    $installedVer = if ($installedLine) { ($installedLine -split '\s+')[-1] } else { $releaseLabel }
    Write-Info "installed liftgate.exe & lg.exe $installedVer -> $targetDir"

    # --- User PATH -----------------------------------------------------------
    if (-not $skipModifyPath) {
      $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
      $parts = @()
      if ($userPath) {
        $parts = @($userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
      }
      $alreadyPresent = $false
      foreach ($entry in $parts) {
        $normEntry = try { Normalize-Dir $entry } catch { $entry.TrimEnd('\', '/') }
        if ([string]::Equals($normEntry, $targetDir, [StringComparison]::OrdinalIgnoreCase)) {
          $alreadyPresent = $true
          break
        }
      }
      if (-not $alreadyPresent) {
        $updatedUserPath = (($parts + $targetDir) -join ';')
        [Environment]::SetEnvironmentVariable('Path', $updatedUserPath, 'User')
        Write-Info "added $targetDir to your User PATH (new terminals will pick it up automatically)."
      }
      $sessionParts = @($env:Path -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
      $inSession = $false
      foreach ($entry in $sessionParts) {
        $normEntry = try { Normalize-Dir $entry } catch { $entry.TrimEnd('\', '/') }
        if ([string]::Equals($normEntry, $targetDir, [StringComparison]::OrdinalIgnoreCase)) {
          $inSession = $true
          break
        }
      }
      if (-not $inSession) {
        $env:Path = "$env:Path;$targetDir"
      }
    }
    else {
      Write-WarnMsg "$targetDir not added to User PATH (-NoModifyPath / LIFTGATE_NO_MODIFY_PATH=1)."
    }

    $resolvedCmd = Get-Command 'liftgate.exe' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($resolvedCmd -and $resolvedCmd.Source -and (-not [string]::Equals((Normalize-Dir (Split-Path -Parent $resolvedCmd.Source)), $targetDir, [StringComparison]::OrdinalIgnoreCase))) {
      Write-WarnMsg "'$($resolvedCmd.Source)' appears earlier on PATH than '$destMain'; place '$targetDir' earlier on PATH if you want this install to take precedence."
    }

    if ($existingExe) {
      Write-Info "done. Run 'lg r' if a background gateway is running, or 'lg --help' to explore."
    }
    else {
      Write-Info "done. Next: run 'lg agy install; lg agy login', then 'lg cx' or 'lg cc'."
    }
  }
  finally {
    Remove-Item -LiteralPath $tmpDir -Recurse -Force -ErrorAction SilentlyContinue
  }
} @args
