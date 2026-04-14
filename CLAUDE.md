# claude-code-yolo

Containerized Claude Code environments using Docker (or Podman) with sandbox command proxying.

## sandbox setup

Build and install the sandbox binary to `~/.local/bin/sandbox` if missing or outdated:

```bash
cd ultra-sandbox/sandbox-rs && cargo build --release && install -m 755 target/release/sandbox ~/.local/bin/sandbox
```

### Static build (for older glibc compatibility)

If you encounter `GLIBC_2.39 not found` errors on older systems, build a static binary using musl:

```bash
# Install musl target (once)
rustup target add x86_64-unknown-linux-musl

# Build static binary
cd ultra-sandbox/sandbox-rs
cargo build --release --target x86_64-unknown-linux-musl

# Install the static binary
install -m 755 target/x86_64-unknown-linux-musl/release/sandbox ~/.local/bin/sandbox
```

For ARM64 systems:
```bash
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

Start the daemon on the host before launching any container:

```bash
sandbox daemon &
```

Map host commands into the container (run from the directory containing `.ultra_sandbox/`):

```bash
cd ultra-sandbox
sandbox map docker
sandbox map adb
sandbox map flutter
```

Then launch the container via the appropriate script:

```bash
bash flutter/sandbox-flutter.sh        # Flutter projects
bash python/sandbox-python.sh          # Python projects
bash ultra-sandbox/ultra-sandbox.sh    # Generic environment
```
