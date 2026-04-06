# Claude Code YOLO

Run Claude Code with all permissions, yet in a secure environment. Let Claude Code fly to the moon.

## Directory Structure

```
claude-code-yolo/
├── python/
│   ├── claude_code_py.Dockerfile      # Python 3.12 + uv + Claude Code image
│   └── claude-yolo-py-docker.sh       # Launch script (for Python projects)
└── flutter/
    ├── claude_code_flutter.Dockerfile  # Ubuntu 22.04 + Flutter + Android SDK + Claude Code image
    └── claude-yolo-flutter-docker.sh  # Launch script (for Flutter projects)
```

## Python YOLO

For Python projects (e.g., AgentFusion).

### Build Image

```bash
cd python
podman build -f claude_code_py.Dockerfile \
    --build-arg HOST_USER_UID=$(id -u) \
    --build-arg HOST_USER_GID=$(id -g) \
    --build-arg HOST_USER_NAME=$USER \
    --build-arg HTTP_PROXY=$HTTP_PROXY \
    --build-arg HTTPS_PROXY=$HTTPS_PROXY \
    -t claude_code_py .
```

### Launch

Run in your Python project root:

```bash
bash /path/to/claude-code-yolo/python/claude-yolo-py-docker.sh
```

**Features:**
- Auto-handles proxy (127.0.0.1 -> host.docker.internal)
- When `.venv` is detected, uses a separate volume to avoid host/container environment conflicts
- Mounts `~/.claude`, `~/.claude.json`, `~/.ssh` (read-only)
- Uses `uv` for Python dependency management, `UV_VENV_CLEAR=1` ensures a clean environment

## Flutter YOLO

For Flutter projects (e.g., ai_tarot).

### Build Image

```bash
cd flutter
podman build -f claude_code_flutter.Dockerfile \
    --build-arg HOST_USER_UID=$(id -u) \
    --build-arg HOST_USER_GID=$(id -g) \
    --build-arg HOST_USER_NAME=$USER \
    --build-arg HTTP_PROXY=$HTTP_PROXY \
    --build-arg HTTPS_PROXY=$HTTPS_PROXY \
    -t claude_code_flutter .
```

Image includes: Flutter 3.41.2, Android SDK (compileSdk 36, NDK 28.2.13676358), OpenJDK 17.

### Launch (with ADB Device Debugging)

First time or after reboot, start ADB server in listen mode on the **host**:

```bash
adb kill-server && adb -a nodaemon server &
```

Then run in your Flutter project root:

```bash
bash /path/to/claude-code-yolo/flutter/claude-yolo-flutter-docker.sh
```

The script will auto-build the image if not found.

**Features:**
- `--network=host`, container ADB client connects directly to host ADB server (127.0.0.1:5037)
- Build directory and .dart_tool use separate volumes to avoid host cache pollution
- Mounts `~/.pub-cache`, `~/.gradle` to share download caches
- Flutter/Pub uses China mirrors (flutter-io.cn)

## General Notes

- Both scripts launch with `claude --dangerously-skip-permissions`, suitable for CI/automation tasks
- Containers use host UID/GID (`--userns=keep-id`), file permissions match the host
- Proxy environment variables (HTTP_PROXY/HTTPS_PROXY, etc.) are automatically passed to the container
