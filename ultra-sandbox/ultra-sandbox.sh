#!/bin/bash

# Ultra Sandbox - Generic containerized development environment
# Usage: ultra-sandbox.sh [command]
# If no command provided, starts an interactive bash shell

set -e

replace_proxy() {
    local proxy="$1"
    echo "${proxy//127.0.0.1:10809/host.docker.internal:10809}"
}

WORK_DIR=$(pwd)
SANDBOX_DIR="$WORK_DIR/.ultra_sandbox"
mkdir -p "$SANDBOX_DIR/bin"
cp "$(command -v sandbox)" "$SANDBOX_DIR/bin/sandbox"

# Cleanup function - remove mapped bins on exit
cleanup() {
    rm -rf "$SANDBOX_DIR"
}
trap cleanup EXIT

echo "Current directory mounted to: $WORK_DIR"

# Detect container engine (podman --userns=keep-id vs docker --user).
if command -v podman &>/dev/null; then
    ENGINE=podman
    USER_ARGS=(--userns=keep-id)
    IMAGE=localhost/claude_code_base:latest
elif command -v docker &>/dev/null; then
    ENGINE=docker
    USER_ARGS=(--user "$(id -u):$(id -g)")
    IMAGE=claude_code_base:latest
else
    echo "Error: need 'podman' or 'docker' on PATH" >&2
    exit 1
fi

"$ENGINE" run -it --rm \
    "${USER_ARGS[@]}" \
    --network=host \
    -v "$WORK_DIR:$WORK_DIR" \
    -v "$HOME/.ssh":"/home/$USER/.ssh:ro" \
    -v "$SANDBOX_DIR":"/ultra_sandbox" \
    -v "$SANDBOX_DIR/bin":"/usr/local/bin":ro \
    -e LANG="$LANG" \
    -e http_proxy="$(replace_proxy "$http_proxy")" \
    -e https_proxy="$(replace_proxy "$https_proxy")" \
    -e HTTP_PROXY="$(replace_proxy "$HTTP_PROXY")" \
    -e HTTPS_PROXY="$(replace_proxy "$HTTPS_PROXY")" \
    -e PATH="/home/$USER/.local/bin:/usr/local/bin:/usr/bin:/bin" \
    -e SANDBOX_DIR="/ultra_sandbox" \
    -e NO_PROXY="$NO_PROXY" \
    -e no_proxy="$no_proxy" \
    -e TERM=xterm-256color \
    -e HOME="/home/$USER" \
    -w "$WORK_DIR" \
    "$IMAGE" \
    "$@"
