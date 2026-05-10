# Ultra-sandbox installer for Windows (PowerShell).
#
# Usage (from any directory — no clone needed):
#   irm https://raw.githubusercontent.com/ZenWayne/ultra-sandbox/main/install.ps1 | iex
# Or from a clone:
#   .\install.ps1
#
# 1. Downloads sandbox.exe from the latest GitHub release.
# 2. Downloads the Dockerfile and builds the claude_code_base image.
# 3. Downloads claude-yolo-automate onto $env:Path.
#
# Env overrides (set before running):
#   $env:INSTALL_DIR      Install destination (default: $env:USERPROFILE\.local\bin)
#   $env:REPO             GitHub repo (default: ZenWayne/ultra-sandbox)
#   $env:BRANCH           Git ref for raw files — branch/tag/sha (default: main)
#   $env:RELEASE_TAG      Sandbox-binary release tag (default: latest)
#   $env:IMAGE_TAG        Built image name (default: claude_code_base)
#   $env:SKIP_SANDBOX     =1 to skip binary download
#   $env:SKIP_IMAGE       =1 to skip image build
#   $env:SKIP_LAUNCHER    =1 to skip launcher install
#
# Note: claude-yolo-automate is a bash script. On native Windows, run it
# from Git Bash, MSYS2, or WSL2. For a pure-bash experience, prefer
# install.sh inside WSL2.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# Detect piped execution (irm | iex) — stdin is not interactive, skip prompts
$Piped = -not [Environment]::UserInteractive -or ($Host.Name -eq 'ServerRemoteHost') -or `
         ($MyInvocation.Line -match 'iex|Invoke-Expression')

$Repo       = if ($env:REPO)        { $env:REPO }        else { 'ZenWayne/ultra-sandbox' }
$Branch     = if ($env:BRANCH)      { $env:BRANCH }      else { 'main' }
$ReleaseTag = if ($env:RELEASE_TAG) { $env:RELEASE_TAG } else { 'latest' }
$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $env:USERPROFILE '.local\bin' }
$ImageTag   = if ($env:IMAGE_TAG)   { $env:IMAGE_TAG }   else { 'claude_code_base' }

$RawBase = "https://raw.githubusercontent.com/$Repo/$Branch"

function Log($msg)  { Write-Host "==> $msg" -ForegroundColor Blue }
function Warn($msg) { Write-Host "[warn] $msg" -ForegroundColor Yellow }
function Die($msg)  { Write-Host "[err] $msg" -ForegroundColor Red; exit 1 }

# Download URL to DEST atomically — works even if DEST is a running binary.
function Fetch($url, $dest) {
    $tmp = "$dest.new.$PID"
    # Honour HTTP_PROXY / HTTPS_PROXY env vars (Invoke-WebRequest ignores them)
    $proxy = $env:HTTPS_PROXY
    if (-not $proxy) { $proxy = $env:HTTP_PROXY }
    $iwrArgs = @{ Uri = $url; OutFile = $tmp; UseBasicParsing = $true }
    if ($proxy) { $iwrArgs['Proxy'] = $proxy; $iwrArgs['ProxyUseDefaultCredentials'] = $true }
    try {
        Invoke-WebRequest @iwrArgs
    } catch {
        if (Test-Path $tmp) { Remove-Item -Force $tmp }
        Die "download failed: $url — $($_.Exception.Message)"
    }
    Move-Item -Force $tmp $dest
}

function Get-Asset {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64'  { return 'sandbox-windows-x86_64.exe' }
        'x86_64' { return 'sandbox-windows-x86_64.exe' }
        default  { Die "unsupported Windows arch: $env:PROCESSOR_ARCHITECTURE (build from source: ultra-sandbox/sandbox-rs)" }
    }
}

function Get-ReleaseUrl($asset) {
    if ($ReleaseTag -eq 'latest') {
        return "https://github.com/$Repo/releases/latest/download/$asset"
    }
    return "https://github.com/$Repo/releases/download/$ReleaseTag/$asset"
}

function Ensure-Dir($path) {
    if (-not (Test-Path $path)) {
        New-Item -ItemType Directory -Path $path -Force | Out-Null
    }
}

function Ensure-Path {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = ($userPath -split ';') | Where-Object { $_ }
    if ($entries -contains $InstallDir) {
        Log "$InstallDir is already on user PATH"
        return
    }
    Log "Adding $InstallDir to user PATH..."
    $newPath = ($userPath.TrimEnd(';') + ";$InstallDir").TrimStart(';')
    [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    $env:Path = "$env:Path;$InstallDir"
    Log "Done. New terminal sessions will pick it up automatically."
}

function Install-Sandbox {
    $asset = Get-Asset
    $url   = Get-ReleaseUrl $asset
    $dest  = Join-Path $InstallDir 'sandbox.exe'

    Log "Downloading $asset"
    Write-Host "      URL:  $url"
    Write-Host "      Dest: $dest"
    Fetch $url $dest
    $size = (Get-Item $dest).Length
    Log "Installed sandbox.exe ($([math]::Round($size / 1KB)) KB)"
}

function Build-Image {
    $engine = $null
    if (Get-Command podman -ErrorAction SilentlyContinue) {
        $engine = 'podman'
    } elseif (Get-Command docker -ErrorAction SilentlyContinue) {
        $engine = 'docker'
    } else {
        Die "need podman (or docker) to build the image. On Windows: install Docker Desktop."
    }

    $hostUser = $env:USERNAME
    if ($hostUser -eq 'root' -or $hostUser -eq 'Administrator') {
        Die "HOST_USER_NAME must not be 'root'/'Administrator' — run installer as a regular user"
    }

    $tmpdir = Join-Path ([System.IO.Path]::GetTempPath()) ("ultra-sandbox-build-" + [System.Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tmpdir -Force | Out-Null
    try {
        Install-File 'ultra-sandbox/claude_code_base.Dockerfile' (Join-Path $tmpdir 'claude_code_base.Dockerfile')

        # Windows has no unix UID/GID; the container runs inside a Linux VM,
        # whose user is typically uid/gid 1000. Hardcode that as the default.
        Log "Building image '$ImageTag' with $engine"
        Write-Host "      Engine:    $engine"
        Write-Host "      User:      $hostUser (UID=1000, GID=1000)"
        Write-Host "      Dockerfile: claude_code_base.Dockerfile"
        if ($env:HTTP_PROXY)  { Write-Host "      HTTP_PROXY:  $($env:HTTP_PROXY)" }
        if ($env:HTTPS_PROXY) { Write-Host "      HTTPS_PROXY: $($env:HTTPS_PROXY)" }
        Write-Host ""
        Push-Location $tmpdir
        try {
            $cacheFlag = if ($env:USE_CACHE -eq '1') { @() } else { @('--no-cache') }
            $buildArgs = @(
                'build') + $cacheFlag + @(
                '-f', 'claude_code_base.Dockerfile',
                '--build-arg', 'HOST_USER_UID=1000',
                '--build-arg', 'HOST_USER_GID=1000',
                '--build-arg', "HOST_USER_NAME=$hostUser",
                '--build-arg', "HTTP_PROXY=$($env:HTTP_PROXY)",
                '--build-arg', "HTTPS_PROXY=$($env:HTTPS_PROXY)",
                '-t', $ImageTag,
                '.'
            )
            & $engine @buildArgs
            if ($LASTEXITCODE -ne 0) { Die "image build failed (exit $LASTEXITCODE)" }
        } finally {
            Pop-Location
        }
        Write-Host ""
        Log "Image built: $ImageTag"
    } finally {
        Remove-Item -Recurse -Force $tmpdir -ErrorAction SilentlyContinue
    }
}

function Install-File($name, $dest) {
    # Prefer local file next to this script (cloned repo), fall back to GitHub
    $scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
    $local = Join-Path $scriptDir ($name -replace '/', '\')
    if (Test-Path $local) {
        Log "Copying $name (local)"
        Copy-Item -Force $local $dest
    } else {
        $url = "$RawBase/$name"
        Log "Downloading $name"
        Write-Host "      URL: $url"
        Fetch $url $dest
    }
    Write-Host "      -> $dest"
}

function Install-Launcher {
    Install-File 'claude-yolo-automate.ps1' (Join-Path $InstallDir 'claude-yolo-automate.ps1')
    Install-File 'claude-yolo-automate.cmd' (Join-Path $InstallDir 'claude-yolo-automate.cmd')
    Log "Installed claude-yolo-automate (.ps1 + .cmd)"
}

function Main {
    Write-Host ""
    Write-Host "  Ultra-Sandbox Installer for Windows" -ForegroundColor Cyan
    Write-Host "  ====================================" -ForegroundColor Cyan
    Write-Host ""
    Write-Host "  Repo:        $Repo"
    Write-Host "  Branch:      $Branch"
    Write-Host "  Release:     $ReleaseTag"
    Write-Host "  Install dir: $InstallDir"
    Write-Host "  Image tag:   $ImageTag"
    Write-Host ""

    $steps = @()
    if ($env:SKIP_SANDBOX  -ne '1') { $steps += "1. Download sandbox.exe (from GitHub Release)" }
    if ($env:SKIP_IMAGE    -ne '1') { $steps += "2. Build Docker image ($ImageTag)" }
    if ($env:SKIP_LAUNCHER -ne '1') { $steps += "3. Install claude-yolo-automate launcher" }

    Write-Host "  This script will:" -ForegroundColor Yellow
    foreach ($s in $steps) { Write-Host "    $s" }
    Write-Host ""

    if (-not $Piped) {
        $answer = Read-Host "  Proceed? [Y/n]"
        if ($answer -and $answer -notmatch '^[Yy]') {
            Write-Host "  Aborted." -ForegroundColor Red
            exit 0
        }
    }
    Write-Host ""

    Ensure-Dir $InstallDir

    if ($env:SKIP_SANDBOX  -ne '1') { Log "[1/3] Downloading sandbox binary...";  Install-Sandbox  } else { Log "[1/3] Skipped (SKIP_SANDBOX=1)" }
    if ($env:SKIP_IMAGE    -ne '1') { Log "[2/3] Building container image...";     Build-Image      } else { Log "[2/3] Skipped (SKIP_IMAGE=1)"   }
    if ($env:SKIP_LAUNCHER -ne '1') { Log "[3/3] Installing launcher script...";   Install-Launcher } else { Log "[3/3] Skipped (SKIP_LAUNCHER=1)" }

    Ensure-Path

    Write-Host ""
    Log "All done!"
    Write-Host ""
    Write-Host "  Next steps:" -ForegroundColor Green
    Write-Host "    cd C:\path\to\your\project"
    Write-Host '    claude-yolo-automate'
    Write-Host ""
    Write-Host "  With command mapping:" -ForegroundColor Green
    Write-Host '    set SANDBOX_MAP_PROCESSES=python docker && claude-yolo-automate'
    Write-Host ""
    Write-Host "  To uninstall:"
    Write-Host "    irm https://raw.githubusercontent.com/$Repo/$Branch/uninstall.ps1 | iex"
    Write-Host ""
}

Main
