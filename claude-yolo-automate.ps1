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

# Convert Windows path to Docker-compatible path: C:\Users\foo -> /c/Users/foo
function To-LinuxPath($p) {
    if ($p -match '^([A-Za-z]):\\') {
        $drive = $Matches[1].ToLower()
        return "/$drive" + ($p.Substring(2) -replace '\\', '/')
    }
    return $p -replace '\\', '/'
}

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

# --- TCP config ---------------------------------------------------------------
$SandboxTcpPort = if ($env:SANDBOX_TCP_PORT) { $env:SANDBOX_TCP_PORT } else { '19999' }
$env:SANDBOX_TCP = '1'
$env:SANDBOX_TCP_ADDR = "127.0.0.1:${SandboxTcpPort}"

# --- Sandbox setup ------------------------------------------------------------
if ($env:SANDBOX_MAP_PROCESSES) {
    $sandboxBin = Get-Command sandbox -ErrorAction SilentlyContinue
    if (-not $sandboxBin) {
        Write-Host "Error: 'sandbox' not found in PATH. Install it first." -ForegroundColor Red
        exit 1
    }

    # On Windows the daemon uses TCP. The Linux sandbox binary is baked into
    # the image. Shims are created at container startup via the entrypoint.
    New-Item -ItemType Directory -Path $SandboxDir -Force | Out-Null

    # Start TCP daemon if not running
    $checkResult = & sandbox daemon-check 2>$null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "Starting sandbox daemon (tcp :${SandboxTcpPort})..."
        Start-Process -FilePath 'sandbox' -ArgumentList 'daemon' -WindowStyle Hidden
        for ($i = 0; $i -lt 10; $i++) {
            Start-Sleep -Milliseconds 100
            & sandbox daemon-check 2>$null
            if ($LASTEXITCODE -eq 0) { break }
        }
    }

    # Write command-map.json directly (daemon uses this for whitelist).
    # On Linux, `sandbox map` creates symlinks + writes this file.
    # On Windows, we skip symlinks (useless for the Linux container) and
    # just write the JSON. Shims are created inside the container at startup.
    $cmds = ($env:SANDBOX_MAP_PROCESSES -split '\s+') | Where-Object { $_ }
    $map = @{}
    foreach ($cmd in $cmds) { $map[$cmd] = $cmd }
    $mapJson = $map | ConvertTo-Json
    [System.IO.File]::WriteAllText((Join-Path $SandboxDir 'command-map.json'), $mapJson, (New-Object System.Text.UTF8Encoding $false))
    Write-Host "=== sandbox mapped: $($cmds -join ' ') ==="

    # Auto-inject build-only podman policy (write policy.json directly)
    $podmanPolicy = if ($env:SANDBOX_PODMAN_POLICY) { $env:SANDBOX_PODMAN_POLICY } else { 'build-only' }
    if ($env:SANDBOX_MAP_PROCESSES -match '\bpodman\b' -and $podmanPolicy -eq 'build-only') {
        $verbs = @('build','images','inspect','version','info','run','exec',
                   'ps','logs','top','stats','port','diff','history','events',
                   'search','wait','exists','compose')
        $policyFile = Join-Path $SandboxDir 'policy.json'
        $existingPolicy = @{}
        if (Test-Path $policyFile) {
            try { $existingPolicy = Get-Content $policyFile -Raw | ConvertFrom-Json -AsHashtable } catch {}
        }
        $allowRules = @()
        foreach ($v in $verbs) { $allowRules += ,@($v) }
        $existingPolicy['podman'] = @{ deny = @(); allow = $allowRules }
        $policyJson = $existingPolicy | ConvertTo-Json -Depth 4
        [System.IO.File]::WriteAllText($policyFile, $policyJson, (New-Object System.Text.UTF8Encoding $false))
        Write-Host "=== podman policy: allow-list = $($verbs -join ' ') ==="
    }
}

# --- Detect container engine --------------------------------------------------
$Engine = $null
$UserArgs = @()
$Image = $null

if (Get-Command docker -ErrorAction SilentlyContinue) {
    $Engine = 'docker'
    $UserArgs = @('--user', '1000:1000')
    $Image = 'claude_code_base:latest'
} elseif (Get-Command podman -ErrorAction SilentlyContinue) {
    $Engine = 'podman'
    $UserArgs = @('--userns=keep-id')
    $Image = 'localhost/claude_code_base:latest'
} else {
    Write-Host "Error: need 'podman' or 'docker' on PATH" -ForegroundColor Red
    exit 1
}

# --- Build volume and env args ------------------------------------------------
$claudeShareVol = if ($env:CLAUDE_SHARE_VOLUME) { $env:CLAUDE_SHARE_VOLUME } else { 'claude_share' }
$claudeBinVol   = if ($env:CLAUDE_BIN_VOLUME)   { $env:CLAUDE_BIN_VOLUME }   else { 'claude_bin' }

# Container-side home dir
$cHome = "/home/$User"

$lWorkDir = To-LinuxPath $WorkDir
$lSandboxDir = To-LinuxPath $SandboxDir

$volumeArgs = @(
    '-v', "${lWorkDir}:${lWorkDir}"
    '-v', "${claudeShareVol}:${cHome}/.local/share/claude"
    '-v', "${claudeBinVol}:${cHome}/.local/bin"
)

$sandboxVolumeArgs = @()
$sandboxEnvArgs = @()

if ($env:SANDBOX_MAP_PROCESSES) {
    $sandboxVolumeArgs = @(
        '-v', "${lSandboxDir}:/ultra_sandbox"
    )
    $cmdsJoined = (($env:SANDBOX_MAP_PROCESSES -split '\s+') | Where-Object { $_ }) -join ' '
    $sandboxEnvArgs = @(
        '-e', 'SANDBOX_DIR=/ultra_sandbox'
        '-e', 'SANDBOX_TCP=1'
        '-e', "SANDBOX_TCP_ADDR=host.docker.internal:${SandboxTcpPort}"
        '-e', "SANDBOX_MAP_CMDS=$cmdsJoined"
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
    $optionalMounts += @('-v', "$(To-LinuxPath $sshDir):${cHome}/.ssh:ro")
}
$claudeDir = Join-Path $Home_ '.claude'
if (Test-Path $claudeDir) {
    $optionalMounts += @('-v', "$(To-LinuxPath $claudeDir):${cHome}/.claude")
}
$claudeJson = Join-Path $Home_ '.claude.json'
if (Test-Path $claudeJson) {
    $optionalMounts += @('-v', "$(To-LinuxPath $claudeJson):${cHome}/.claude.json")
}
$screenshotDir = Join-Path $Home_ 'Pictures\screenshot'
if (Test-Path $screenshotDir) {
    $optionalMounts += @('-v', "$(To-LinuxPath $screenshotDir):${cHome}/Pictures/screenshot")
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
# Entrypoint: create symlink shims inside the container, then exec claude.
# Each mapped command gets a symlink: /ultra_sandbox/bin/<cmd> -> /usr/local/bin/sandbox
# The sandbox binary detects argv[0] and acts as a client for that command.
$entryScript = 'mkdir -p /ultra_sandbox/bin; for cmd in $SANDBOX_MAP_CMDS; do ln -sf /usr/local/bin/sandbox /ultra_sandbox/bin/$cmd; done; exec "$HOME/.local/bin/claude" --dangerously-skip-permissions "$@"'

$entrypointArgs = @()
if ($env:SANDBOX_MAP_PROCESSES) {
    $entrypointArgs = @('--entrypoint', '/bin/sh', $Image, '-c', $entryScript, '--')
} else {
    $entrypointArgs = @('--entrypoint', "${cHome}/.local/bin/claude", $Image, '--dangerously-skip-permissions')
}

try {
    & $Engine run -it --rm `
        @UserArgs `
        --network=host `
        @volumeArgs `
        @sandboxVolumeArgs `
        @optionalMounts `
        @envArgs `
        @sandboxEnvArgs `
        -w $lWorkDir `
        @entrypointArgs @args
} finally {
    Cleanup
}
