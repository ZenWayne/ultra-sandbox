#!/bin/bash
# Flutter + Claude YOLO with sandbox command proxy
# Mapped commands: flutter, adb, podman

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export SANDBOX_DIR="$SCRIPT_DIR/../ultra-sandbox/.ultra_sandbox"
IMAGE="localhost/claude_code_flutter:latest"

replace_proxy() {
    local proxy="$1"
    echo "${proxy//127.0.0.1:10809/host.docker.internal:10809}"
}

# --- Ensure sandbox binary is installed -------------------------------------
if ! command -v sandbox &>/dev/null; then
    echo "Error: 'sandbox' not found in PATH. Install it to ~/.local/bin/sandbox first."
    exit 1
fi

# --- Ensure daemon is running ------------------------------------------------
mkdir -p "$SANDBOX_DIR/bin"
if [ ! -S "$SANDBOX_DIR/daemon.sock" ]; then
    echo "Starting sandbox daemon..."
    sandbox daemon &
    sleep 0.3
fi

# --- Map host commands -------------------------------------------------------
sandbox map flutter
sandbox map adb
sandbox map podman

echo "=== sandbox mapped: flutter, adb, podman ==="

# --- Auto-build image if missing ---------------------------------------------
if ! podman image exists "$IMAGE" 2>/dev/null; then
    echo "=== Image '$IMAGE' not found, building... ==="
    podman build \
        -f "$SCRIPT_DIR/claude_code_flutter.Dockerfile" \
        --build-arg HOST_USER_UID="$(id -u)" \
        --build-arg HOST_USER_GID="$(id -g)" \
        --build-arg HOST_USER_NAME="$USER" \
        --build-arg HTTP_PROXY="$(replace_proxy "$HTTP_PROXY")" \
        --build-arg HTTPS_PROXY="$(replace_proxy "$HTTPS_PROXY")" \
        -t "$IMAGE" \
        "$SCRIPT_DIR"
fi

# --- Launch container --------------------------------------------------------
WORK_DIR=$(pwd)
WORK_DIR_ESCAPED="${WORK_DIR//\//_}"

podman run -it --rm \
    --userns=keep-id \
    --network=host \
    -v "$WORK_DIR:$WORK_DIR" \
    -v "$HOME/.claude":"/home/$USER/.claude" \
    -v "$HOME/.claude.json":"/home/$USER/.claude.json" \
    -v "$HOME/.ssh":"/home/$USER/.ssh:ro" \
    -v "$HOME/.pub-cache":"/home/$USER/.pub-cache" \
    -v "$HOME/.gradle":"/home/$USER/.gradle" \
    -v "/tmp":"/tmp" \
    -v "flutter_build_${WORK_DIR_ESCAPED}:$WORK_DIR/build" \
    -v "flutter_dart_tool_${WORK_DIR_ESCAPED}:$WORK_DIR/.dart_tool" \
    -v "$SANDBOX_DIR":"/ultra_sandbox" \
    -v "$HOME/.local/bin/sandbox":"/usr/local/bin/sandbox:ro" \
    -e ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" \
    -e ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
    -e DISABLE_AUTOUPDATER=1 \
    -e SANDBOX_DIR="/ultra_sandbox" \
    -e LANG="$LANG" \
    -e LC_ALL="$LC_ALL" \
    -e FLUTTER_STORAGE_BASE_URL="https://storage.flutter-io.cn" \
    -e PUB_HOSTED_URL="https://pub.flutter-io.cn" \
    -e http_proxy="$(replace_proxy "$http_proxy")" \
    -e https_proxy="$(replace_proxy "$https_proxy")" \
    -e HTTP_PROXY="$(replace_proxy "$HTTP_PROXY")" \
    -e HTTPS_PROXY="$(replace_proxy "$HTTPS_PROXY")" \
    -e NO_PROXY="$NO_PROXY" \
    -e no_proxy="$no_proxy" \
    -e TERM=xterm-256color \
    -e HOME="/home/$USER" \
    -e PATH="/ultra_sandbox/bin:/opt/flutter/bin:/opt/android-sdk/platform-tools:/opt/android-sdk/cmdline-tools/latest/bin:/usr/local/bin:/usr/bin:/bin" \
    -w "$WORK_DIR" \
    "$IMAGE" \
    claude --dangerously-skip-permissions "$@"
