#!/bin/bash
# Claude YOLO mode for Flutter projects with ADB via host ADB server
#
# Prerequisites (run once on host before starting container):
#   adb kill-server && adb -a nodaemon server &
#
# With --network=host, the container shares the host's network namespace.
# The container's ADB client connects to 127.0.0.1:5037 which is the host's
# ADB server. No special ANDROID_ADB_SERVER_ADDRESS needed.

set -e

IMAGE="localhost/claude_code_flutter:latest"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Helper: replace 127.0.0.1 proxy with host.docker.internal
replace_proxy() {
    local proxy="$1"
    echo "${proxy//127.0.0.1:10809/host.docker.internal:10809}"
}

# ─── Auto-build if image doesn't exist ──────────────────────
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
    echo "=== Build complete ==="
fi

WORK_DIR=$(pwd)
WORK_DIR_ESCAPED="${WORK_DIR//\//_}"

# Volume args: mount current directory at same path inside container
VOLUME_ARGS=(-v "$WORK_DIR:$WORK_DIR")

# Remind user to ensure host ADB server is running in -a mode
echo "=== Flutter ADB Setup ==="
echo "Make sure host ADB server is running in network mode:"
echo "  adb kill-server && adb -a nodaemon server &"
echo ""
echo "Current ADB devices on host:"
adb devices 2>/dev/null || echo "(adb not found or no devices)"
echo "========================="
echo ""

podman run -it --rm \
    --userns=keep-id \
    --network=host \
    "${VOLUME_ARGS[@]}" \
    -v "$HOME/.claude":"/home/$USER/.claude" \
    -v "$HOME/.claude.json":"/home/$USER/.claude.json" \
    -v "$HOME/.ssh":"/home/$USER/.ssh:ro" \
    -v "$HOME/.pub-cache":"/home/$USER/.pub-cache" \
    -v "$HOME/.gradle":"/home/$USER/.gradle" \
    -v "flutter_build_${WORK_DIR_ESCAPED}:$WORK_DIR/build" \
    -v "flutter_dart_tool_${WORK_DIR_ESCAPED}:$WORK_DIR/.dart_tool" \
    -e ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" \
    -e ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
    -e DISABLE_AUTOUPDATER=1 \
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
    -e PATH="/opt/flutter/bin:/opt/android-sdk/platform-tools:/opt/android-sdk/cmdline-tools/latest/bin:/usr/local/bin:/usr/bin:/bin" \
    -w "$WORK_DIR" \
    "$IMAGE" \
    claude --dangerously-skip-permissions "$@"
