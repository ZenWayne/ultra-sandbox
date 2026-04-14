#!/bin/bash

# Helper function to replace 127.0.0.1:10809 with host.docker.internal:10809
replace_proxy() {
    local proxy="$1"
    echo "${proxy//127.0.0.1:10809/host.docker.internal:10809}"
}

# Get current directory
WORK_DIR=$(pwd)

# Build volume mount arguments
VOLUME_ARGS=(-v "$WORK_DIR:$WORK_DIR")

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
    "${VOLUME_ARGS[@]}" \
    -v "$HOME/.claude":"/home/$USER/.claude" \
    -v "$HOME/.claude.json":"/home/$USER/.claude.json" \
    -v "$HOME/.ssh":"/home/$USER/.ssh:ro" \
    -e ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" \
    -e ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
    -e DISABLE_AUTOUPDATER=1 \
    -e LANG="$LANG" \
    -e http_proxy="$(replace_proxy "$http_proxy")" \
    -e https_proxy="$(replace_proxy "$https_proxy")" \
    -e HTTP_PROXY="$(replace_proxy "$HTTP_PROXY")" \
    -e HTTPS_PROXY="$(replace_proxy "$HTTPS_PROXY")" \
    -e NO_PROXY="$NO_PROXY" \
    -e no_proxy="$no_proxy" \
    -e TERM=xterm-256color \
    -e HOME="/home/$USER" \
    -w "$WORK_DIR" \
    --entrypoint /home/$USER/.local/bin/claude \
    "$IMAGE" \
    --dangerously-skip-permissions "$@"
