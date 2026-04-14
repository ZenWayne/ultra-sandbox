#!/usr/bin/env bash
# Ultra-sandbox installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ZenWayne/ultra-sandbox/main/install.sh | bash
# Or from a clone:
#   ./install.sh
#
# 1. Downloads the `sandbox` binary from the latest GitHub release.
# 2. Downloads the Dockerfile and builds the `claude_code_base` image.
# 3. Downloads `claude-yolo-automate` onto $PATH.
#
# Env overrides:
#   INSTALL_DIR       Install destination (default: $HOME/.local/bin)
#   REPO              GitHub repo (default: ZenWayne/ultra-sandbox)
#   BRANCH            Git ref for raw files — branch/tag/sha (default: main)
#   RELEASE_TAG       Sandbox-binary release tag (default: latest)
#   IMAGE_TAG         Built image name (default: claude_code_base)
#   SKIP_SANDBOX      =1 to skip sandbox binary download
#   SKIP_IMAGE        =1 to skip image build
#   SKIP_LAUNCHER     =1 to skip launcher install

set -euo pipefail

REPO="${REPO:-ZenWayne/ultra-sandbox}"
BRANCH="${BRANCH:-main}"
RELEASE_TAG="${RELEASE_TAG:-latest}"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
IMAGE_TAG="${IMAGE_TAG:-claude_code_base}"

RAW_BASE="https://raw.githubusercontent.com/$REPO/$BRANCH"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$*" >&2; }
err()  { printf '\033[1;31m[err]\033[0m %s\n' "$*" >&2; exit 1; }

# fetch URL DEST  — download URL to DEST atomically (via temp file + mv), so
# even a binary that's currently executing can be replaced (ETXTBSY-safe).
fetch() {
    local url="$1" dest="$2" tmp="$2.new.$$"
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 -o "$tmp" "$url" || { rm -f "$tmp"; err "download failed: $url"; }
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$tmp" "$url" || { rm -f "$tmp"; err "download failed: $url"; }
    else
        err "need curl or wget to download files"
    fi
    mv -f "$tmp" "$dest"
}

detect_asset() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux)
            case "$arch" in
                x86_64|amd64) echo "sandbox-linux-x86_64" ;;
                *) err "unsupported Linux arch: $arch (build from source: ultra-sandbox/sandbox-rs)" ;;
            esac ;;
        Darwin)
            case "$arch" in
                arm64|aarch64) echo "sandbox-darwin-arm64" ;;
                *) err "unsupported macOS arch: $arch (Intel macs must build from source)" ;;
            esac ;;
        MINGW*|MSYS*|CYGWIN*) echo "sandbox-windows-x86_64.exe" ;;
        *) err "unsupported OS: $os" ;;
    esac
}

release_url() {
    local asset="$1"
    if [ "$RELEASE_TAG" = "latest" ]; then
        echo "https://github.com/$REPO/releases/latest/download/$asset"
    else
        echo "https://github.com/$REPO/releases/download/$RELEASE_TAG/$asset"
    fi
}

ensure_dir() {
    [ -d "$1" ] || mkdir -p "$1"
}

check_path() {
    case ":$PATH:" in
        *":$INSTALL_DIR:"*) return 0 ;;
        *) warn "$INSTALL_DIR is not on \$PATH — add it to your shell rc:"
           warn "  export PATH=\"$INSTALL_DIR:\$PATH\""
           return 1 ;;
    esac
}

install_sandbox() {
    local asset url dest
    asset="$(detect_asset)"
    url="$(release_url "$asset")"
    dest="$INSTALL_DIR/sandbox"
    case "$asset" in *.exe) dest="$dest.exe" ;; esac

    log "Downloading $asset from $url"
    fetch "$url" "$dest"
    chmod +x "$dest"
    log "Installed sandbox -> $dest"
}

build_image() {
    local engine=""
    if command -v podman >/dev/null 2>&1; then
        engine="podman"
    elif command -v docker >/dev/null 2>&1; then
        engine="docker"
    else
        err "need podman (or docker) to build the image"
    fi

    local host_user="${USER:-$(id -un)}"
    if [ "$host_user" = "root" ]; then
        err "HOST_USER_NAME must not be 'root' — run installer as a non-root user"
    fi

    local tmpdir
    tmpdir="$(mktemp -d)"
    trap 'rm -rf "$tmpdir"' RETURN

    log "Fetching Dockerfile"
    fetch "$RAW_BASE/ultra-sandbox/claude_code_base.Dockerfile" "$tmpdir/claude_code_base.Dockerfile"

    log "Building image $IMAGE_TAG with $engine"
    (
        cd "$tmpdir"
        "$engine" build \
            -f claude_code_base.Dockerfile \
            --build-arg HOST_USER_UID="$(id -u)" \
            --build-arg HOST_USER_GID="$(id -g)" \
            --build-arg HOST_USER_NAME="$host_user" \
            --build-arg HTTP_PROXY="${HTTP_PROXY:-}" \
            --build-arg HTTPS_PROXY="${HTTPS_PROXY:-}" \
            -t "$IMAGE_TAG" \
            .
    )
    log "Image built: $IMAGE_TAG"
}

install_launcher() {
    local dest="$INSTALL_DIR/claude-yolo-automate"
    log "Fetching claude-yolo-automate -> $dest"
    fetch "$RAW_BASE/claude-yolo-automate" "$dest"
    chmod +x "$dest"
}

main() {
    ensure_dir "$INSTALL_DIR"

    if [ "${SKIP_SANDBOX:-0}" != "1" ]; then
        install_sandbox
    else
        log "Skipping sandbox download (SKIP_SANDBOX=1)"
    fi

    if [ "${SKIP_IMAGE:-0}" != "1" ]; then
        build_image
    else
        log "Skipping image build (SKIP_IMAGE=1)"
    fi

    if [ "${SKIP_LAUNCHER:-0}" != "1" ]; then
        install_launcher
    else
        log "Skipping launcher install (SKIP_LAUNCHER=1)"
    fi

    check_path || true

    log "Done."
    cat <<EOF

Next steps:
  cd /path/to/your/project
  SANDBOX_MAP_PROCESSES="python" claude-yolo-automate

Override mapped commands via SANDBOX_MAP_PROCESSES, e.g.:
  SANDBOX_MAP_PROCESSES="python npx" claude-yolo-automate
EOF
}

main "$@"
