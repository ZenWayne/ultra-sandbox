# Claude Code - auto-maps host commands listed in SANDBOX_MAP_PROCESSES
#
# Usage (CMD):   claude-yolo-automate
# Usage (PS):    .\claude-yolo-automate.ps1
#
# Environment variables:
#   SANDBOX_MAP_PROCESSES   space-separated commands to proxy (e.g. "podman adb")
#   SANDBOX_DIR             shim/socket dir (default: $pwd\.ultra_sandbox)
#   SANDBOX_PODMAN_POLICY   "build-only" (default) or "none"

$ErrorActionPreference = 'Stop'

$WorkDir = (Get-Location).Path
$User = $env:USERNAME
$Home_ = $env:USERPROFILE

if (-not $env:SANDBOX_DIR) {
    $env:SANDBOX_DIR = Join-Path $WorkDir '.ultra_sandbox'
}
$SandboxDir = $env:SANDBOX_DIR

# --- PID tracking for cleanup ------------------------------------------------
New-Item -ItemType Directory -Path $SandboxDir -Force | Out-Null
$pidFile = Join-Path $SandboxDir 'pids'
Add-Content -Path $pidFile -Value $PID

function Cleanup {
    if (Test-Path $pidFile) {
        $pids = Get-Content $pidFile | Where-Object { $_ -ne "$PID" -and $_ -ne '' }
        if ($pids) {
            Set-Content -Path $pidFile -Value $pids
        } else {
            Remove-Item -Recurse -Force $SandboxDir -ErrorAction SilentlyContinue
        }
    }
}

# --- Sandbox setup ------------------------------------------------------------
if ($env:SANDBOX_MAP_PROCESSES) {
    $sandboxBin = Get-Command sandbox -ErrorAction SilentlyContinue
    if (-not $sandboxBin) {
        Write-Host "Error: 'sandbox' not found in PATH. Install it first." -ForegroundColor Red
        exit 1
    }

    $binDir = Join-Path $SandboxDir 'bin'
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null

    # Copy sandbox binary into sandbox dir
    $src = $sandboxBin.Source
    $dst = Join-Path $binDir 'sandbox.exe'
    if ((-not (Test-Path $dst)) -or ((Get-Item $src).LastWriteTime -gt (Get-Item $dst).LastWriteTime)) {
        Copy-Item -Force $src $dst
    }

    # Start daemon if not running
    $env:SANDBOX_BIN_DIR = $binDir
    $checkResult = & sandbox daemon-check 2>$null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Starting sandbox daemon..."
        Start-Process -FilePath 'sandbox' -ArgumentList 'daemon' -WindowStyle Hidden
        for ($i = 0; $i -lt 10; $i++) {
            Start-Sleep -Milliseconds 100
            & sandbox daemon-check 2>$null
            if ($LASTEXITCODE -eq 0) { break }
        }
    }

    # Map commands
    foreach ($cmd in ($env:SANDBOX_MAP_PROCESSES -split '\s+')) {
        if ($cmd) {
            & sandbox map $cmd
        }
    }
    Write-Host "=== sandbox mapped: $($env:SANDBOX_MAP_PROCESSES) ==="

    # Auto-inject build-only podman policy
    $podmanPolicy = if ($env:SANDBOX_PODMAN_POLICY) { $env:SANDBOX_PODMAN_POLICY } else { 'build-only' }
    if ($env:SANDBOX_MAP_PROCESSES -match '\bpodman\b' -and $podmanPolicy -eq 'build-only') {
        $verbs = @('build','images','inspect','version','info','run','exec',
                   'ps','logs','top','stats','port','diff','history','events',
                   'search','wait','exists','compose')
        foreach ($v in $verbs) {
            & sandbox policy allow podman $v 2>$null
        }
        Write-Host "=== podman policy: allow-list = $($verbs -join ' ') ==="
    }
}

# --- Detect container engine --------------------------------------------------
$Engine = $null
$UserArgs = @()
$Image = $null

if (Get-Command podman -ErrorAction SilentlyContinue) {
    $Engine = 'podman'
    $UserArgs = @('--userns=keep-id')
    $Image = 'localhost/claude_code_base:latest'
} elseif (Get-Command docker -ErrorAction SilentlyContinue) {
    $Engine = 'docker'
    $UserArgs = @('--user', '1000:1000')
    $Image = 'claude_code_base:latest'
} else {
    Write-Host "Error: need 'podman' or 'docker' on PATH" -ForegroundColor Red
    exit 1
}

# --- Build volume and env args ------------------------------------------------
$claudeShareVol = if ($env:CLAUDE_SHARE_VOLUME) { $env:CLAUDE_SHARE_VOLUME } else { 'claude_share' }
$claudeBinVol   = if ($env:CLAUDE_BIN_VOLUME)   { $env:CLAUDE_BIN_VOLUME }   else { 'claude_bin' }

# Container-side home dir
$cHome = "/home/$User"

$volumeArgs = @(
    '-v', "${WorkDir}:${WorkDir}"
    '-v', "${claudeShareVol}:${cHome}/.local/share/claude"
    '-v', "${claudeBinVol}:${cHome}/.local/bin"
)

$sandboxVolumeArgs = @()
$sandboxEnvArgs = @()

if ($env:SANDBOX_MAP_PROCESSES) {
    $sandboxBinPath = (Get-Command sandbox).Source
    $sandboxVolumeArgs = @(
        '-v', "${SandboxDir}:/ultra_sandbox"
        '-v', "${sandboxBinPath}:/usr/local/bin/sandbox:ro"
    )
    $sandboxEnvArgs = @(
        '-e', 'SANDBOX_DIR=/ultra_sandbox'
        '-e', "PATH=/ultra_sandbox/bin:${cHome}/.local/bin:/usr/local/bin:/usr/bin:/bin"
    )
} else {
    $sandboxEnvArgs = @(
        '-e', "PATH=${cHome}/.local/bin:/usr/local/bin:/usr/bin:/bin"
    )
}

# Mount ~/.ssh, ~/.claude, ~/.claude.json if they exist
$optionalMounts = @()
$sshDir = Join-Path $Home_ '.ssh'
if (Test-Path $sshDir) {
    $optionalMounts += @('-v', "${sshDir}:${cHome}/.ssh:ro")
}
$claudeDir = Join-Path $Home_ '.claude'
if (Test-Path $claudeDir) {
    $optionalMounts += @('-v', "${claudeDir}:${cHome}/.claude")
}
$claudeJson = Join-Path $Home_ '.claude.json'
if (Test-Path $claudeJson) {
    $optionalMounts += @('-v', "${claudeJson}:${cHome}/.claude.json")
}
$screenshotDir = Join-Path $Home_ 'Pictures\screenshot'
if (Test-Path $screenshotDir) {
    $optionalMounts += @('-v', "${screenshotDir}:${cHome}/Pictures/screenshot")
}

# Environment passthrough
$envArgs = @(
    '-e', "HOME=${cHome}"
    '-e', "TERM=xterm-256color"
    '-e', "LANG=$($env:LANG)"
)
foreach ($var in @('ANTHROPIC_BASE_URL','ANTHROPIC_API_KEY',
                   'http_proxy','https_proxy','HTTP_PROXY','HTTPS_PROXY',
                   'NO_PROXY','no_proxy')) {
    $val = [Environment]::GetEnvironmentVariable($var)
    if ($val) {
        $envArgs += @('-e', "${var}=${val}")
    }
}

# --- Prune stale claude versions ----------------------------------------------
if (-not $env:SKIP_CLAUDE_PRUNE) {
    & $Engine run --rm @UserArgs `
        -v "${claudeShareVol}:${cHome}/.local/share/claude" `
        -v "${claudeBinVol}:${cHome}/.local/bin" `
        -e "HOME=${cHome}" `
        --entrypoint /bin/bash `
        $Image `
        -c 'set -e
            share="$HOME/.local/share/claude/versions"
            bin="$HOME/.local/bin/claude"
            [ -L "$bin" ] || exit 0
            target=$(readlink -f "$bin") || exit 0
            [ -d "$target" ] || exit 0
            current=$(basename "$target")
            for d in "$share"/*/; do
                [ -d "$d" ] || continue
                v=$(basename "$d")
                if [ "$v" != "$current" ]; then
                    echo "claude-yolo-automate: pruning stale claude version $v"
                    rm -rf "$d"
                fi
            done' 2>$null
}

# --- Launch container ---------------------------------------------------------
try {
    & $Engine run -it --rm `
        @UserArgs `
        --network=host `
        @volumeArgs `
        @sandboxVolumeArgs `
        @optionalMounts `
        @envArgs `
        @sandboxEnvArgs `
        -w $WorkDir `
        --entrypoint "${cHome}/.local/bin/claude" `
        $Image `
        --dangerously-skip-permissions @args
} finally {
    Cleanup
}
