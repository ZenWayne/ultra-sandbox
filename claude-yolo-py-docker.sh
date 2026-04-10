#!/bin/bash
# Python + Claude YOLO with sandbox command proxy
# Mapped commands: podman

set -e

WORK_DIR=$(pwd)
export SANDBOX_DIR="$WORK_DIR/.ultra_sandbox"

# Cleanup function - remove sandbox dir on exit
cleanup() {
    rm -rf "$SANDBOX_DIR"
}
trap cleanup EXIT

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
sandbox map podman

echo "=== sandbox mapped: podman ==="

# --- Launch container --------------------------------------------------------
VOLUME_NAME="claude-yolo-$(echo "$WORK_DIR" | sed 's/[^a-zA-Z0-9]/_/g' | tr '[:upper:]' '[:lower:]')"
VOLUME_ARGS=(-v "$WORK_DIR:$WORK_DIR")

if [ -d ".venv" ]; then
    echo "Detected .venv — container will use an isolated venv: ${VOLUME_NAME}_venv"
    VOLUME_ARGS+=(-v "${VOLUME_NAME}_venv:$WORK_DIR/.venv")
fi

podman run -it --rm \
    --userns=keep-id \
    --network=host \
    "${VOLUME_ARGS[@]}" \
    -v "$HOME/.claude":"/home/$USER/.claude" \
    -v "$HOME/.claude.json":"/home/$USER/.claude.json" \
    -v "$HOME/.ssh":"/home/$USER/.ssh:ro" \
    -v "$SANDBOX_DIR":"/ultra_sandbox" \
    -v "$HOME/.local/bin/sandbox":"/usr/local/bin/sandbox:ro" \
    -e ANTHROPIC_BASE_URL="$ANTHROPIC_BASE_URL" \
    -e ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY" \
    -e DISABLE_AUTOUPDATER=1 \
    -e SANDBOX_DIR="/ultra_sandbox" \
    -e LANG="$LANG" \
    -e http_proxy="$(replace_proxy "$http_proxy")" \
    -e https_proxy="$(replace_proxy "$https_proxy")" \
    -e HTTP_PROXY="$(replace_proxy "$HTTP_PROXY")" \
    -e HTTPS_PROXY="$(replace_proxy "$HTTPS_PROXY")" \
    -e NO_PROXY="$NO_PROXY" \
    -e no_proxy="$no_proxy" \
    -e TERM=xterm-256color \
    -e HOME="/home/$USER" \
    -e PATH="/ultra_sandbox/bin:/home/$USER/.local/bin:/usr/local/bin:/usr/bin:/bin" \
    -e UV_VENV_CLEAR=1 \
    -w "$WORK_DIR" \
    --entrypoint /home/$USER/.local/bin/claude \
    localhost/claude_code_py:latest \
    --dangerously-skip-permissions "$@"
