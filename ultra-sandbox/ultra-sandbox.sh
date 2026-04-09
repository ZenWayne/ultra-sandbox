#!/bin/bash

# Ultra Sandbox - Generic containerized development environment
# Usage: ultra-sandbox.sh [command]
# If no command provided, starts an interactive bash shell

replace_proxy() {
    local proxy="$1"
    echo "${proxy//127.0.0.1:10809/host.docker.internal:10809}"
}

WORK_DIR=$(pwd)
SANDBOX_DIR="$(dirname "$(realpath "$0")")/.ultra_sandbox"
mkdir -p "$SANDBOX_DIR/bin"

echo "Current directory mounted to: $WORK_DIR"

podman run -it --rm \
    --userns=keep-id \
    --network=host \
    -v "$WORK_DIR:$WORK_DIR" \
    -v "$HOME/.ssh":"/home/$USER/.ssh:ro" \
    -v "$SANDBOX_DIR":"/ultra_sandbox" \
    -v "$HOME/.local/bin/sandbox":"/usr/local/bin/sandbox:ro" \
    -e LANG="$LANG" \
    -e LC_ALL="$LC_ALL" \
    -e http_proxy="$(replace_proxy "$http_proxy")" \
    -e https_proxy="$(replace_proxy "$https_proxy")" \
    -e HTTP_PROXY="$(replace_proxy "$HTTP_PROXY")" \
    -e HTTPS_PROXY="$(replace_proxy "$HTTPS_PROXY")" \
    -e PATH="/ultra_sandbox/bin:/home/$USER/.local/bin:/usr/local/bin:/usr/bin:/bin" \
    -e SANDBOX_DIR="/ultra_sandbox" \
    -e NO_PROXY="$NO_PROXY" \
    -e no_proxy="$no_proxy" \
    -e TERM=xterm-256color \
    -e HOME="/home/$USER" \
    -w "$WORK_DIR" \
    localhost/claude_code_base:latest \
    "$@"
