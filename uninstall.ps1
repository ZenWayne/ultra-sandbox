# Ultra-sandbox uninstaller for Windows (PowerShell).
#
# Usage:
#   irm https://raw.githubusercontent.com/ZenWayne/ultra-sandbox/main/uninstall.ps1 | iex
# Or from CMD:
#   powershell -c "irm https://raw.githubusercontent.com/ZenWayne/ultra-sandbox/main/uninstall.ps1 | iex"

$ErrorActionPreference = 'Stop'

$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $env:USERPROFILE '.local\bin' }
$ImageTag   = if ($env:IMAGE_TAG)   { $env:IMAGE_TAG }   else { 'claude_code_base' }

function Log($msg)  { Write-Host "==> $msg" -ForegroundColor Blue }
function Warn($msg) { Write-Host "[warn] $msg" -ForegroundColor Yellow }

Write-Host ""
Write-Host "  Ultra-Sandbox Uninstaller" -ForegroundColor Cyan
Write-Host "  =========================" -ForegroundColor Cyan
Write-Host ""
Write-Host "  Install dir: $InstallDir"
Write-Host "  Image tag:   $ImageTag"
Write-Host ""
Write-Host "  This will remove:" -ForegroundColor Yellow
Write-Host "    1. sandbox.exe"
Write-Host "    2. claude-yolo-automate.ps1 + .cmd"
Write-Host "    3. Docker image $ImageTag"
Write-Host "    4. Docker volumes claude_share, claude_bin"
Write-Host "    5. $InstallDir from user PATH (if present)"
Write-Host ""

$answer = Read-Host "  Proceed? [y/N]"
if ($answer -notmatch '^[Yy]') {
    Write-Host "  Aborted." -ForegroundColor Red
    exit 0
}
Write-Host ""

# 1. Remove binaries
$files = @('sandbox.exe', 'claude-yolo-automate.ps1', 'claude-yolo-automate.cmd')
foreach ($f in $files) {
    $p = Join-Path $InstallDir $f
    if (Test-Path $p) {
        Remove-Item -Force $p
        Log "Removed $p"
    }
}

# 2. Remove Docker image
$engine = $null
if (Get-Command docker -ErrorAction SilentlyContinue) { $engine = 'docker' }
elseif (Get-Command podman -ErrorAction SilentlyContinue) { $engine = 'podman' }

if ($engine) {
    $imageExists = & $engine images -q $ImageTag 2>$null
    if ($imageExists) {
        & $engine rmi $ImageTag 2>$null
        Log "Removed image $ImageTag"
    } else {
        Warn "Image $ImageTag not found, skipping"
    }

    # 3. Remove named volumes
    foreach ($vol in @('claude_share', 'claude_bin')) {
        $volExists = & $engine volume inspect $vol 2>$null
        if ($LASTEXITCODE -eq 0) {
            & $engine volume rm $vol 2>$null
            Log "Removed volume $vol"
        }
    }
} else {
    Warn "No container engine found, skipping image/volume cleanup"
}

# 4. Remove from PATH
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($userPath) {
    $entries = ($userPath -split ';') | Where-Object { $_ -and $_ -ne $InstallDir }
    $newPath = $entries -join ';'
    if ($newPath -ne $userPath) {
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Log "Removed $InstallDir from user PATH"
    }
}

# 5. Remove install dir if empty
if ((Test-Path $InstallDir) -and -not (Get-ChildItem $InstallDir)) {
    Remove-Item $InstallDir
    Log "Removed empty directory $InstallDir"
}

Write-Host ""
Log "Uninstall complete."
Write-Host ""
