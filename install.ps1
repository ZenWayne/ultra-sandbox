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
    try {
        Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
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

function Check-Path {
    $paths = ($env:Path -split ';') | Where-Object { $_ }
    if ($paths -contains $InstallDir) { return }
    Warn "$InstallDir is not on `$env:Path — add it to your user PATH:"
    Warn "  [Environment]::SetEnvironmentVariable('Path', `"`$env:Path;$InstallDir`", 'User')"
}

function Install-Sandbox {
    $asset = Get-Asset
    $url   = Get-ReleaseUrl $asset
    $dest  = Join-Path $InstallDir 'sandbox.exe'

    Log "Downloading $asset from $url"
    Fetch $url $dest
    Log "Installed sandbox -> $dest"
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
        Log "Fetching Dockerfile"
        Fetch "$RawBase/ultra-sandbox/claude_code_base.Dockerfile" (Join-Path $tmpdir 'claude_code_base.Dockerfile')

        # Windows has no unix UID/GID; the container runs inside a Linux VM,
        # whose user is typically uid/gid 1000. Hardcode that as the default.
        Log "Building image $ImageTag with $engine"
        Push-Location $tmpdir
        try {
            $buildArgs = @(
                'build', '-f', 'claude_code_base.Dockerfile',
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
        Log "Image built: $ImageTag"
    } finally {
        Remove-Item -Recurse -Force $tmpdir -ErrorAction SilentlyContinue
    }
}

function Install-Launcher {
    $dest = Join-Path $InstallDir 'claude-yolo-automate'
    Log "Fetching claude-yolo-automate -> $dest"
    Fetch "$RawBase/claude-yolo-automate" $dest
    Warn "claude-yolo-automate is a bash script — on native Windows, run it from Git Bash, MSYS2, or WSL2."
}

function Main {
    Ensure-Dir $InstallDir

    if ($env:SKIP_SANDBOX -ne '1') { Install-Sandbox } else { Log "Skipping sandbox download (SKIP_SANDBOX=1)" }
    if ($env:SKIP_IMAGE   -ne '1') { Build-Image     } else { Log "Skipping image build (SKIP_IMAGE=1)"     }
    if ($env:SKIP_LAUNCHER -ne '1') { Install-Launcher } else { Log "Skipping launcher install (SKIP_LAUNCHER=1)" }

    Check-Path

    Log "Done."
    Write-Host ""
    Write-Host "Next steps (from Git Bash / MSYS2 / WSL2):"
    Write-Host "  cd /path/to/your/project"
    Write-Host '  SANDBOX_MAP_PROCESSES="python" claude-yolo-automate'
    Write-Host ""
    Write-Host "Override mapped commands via SANDBOX_MAP_PROCESSES, e.g.:"
    Write-Host '  SANDBOX_MAP_PROCESSES="python npx" claude-yolo-automate'
}

Main
