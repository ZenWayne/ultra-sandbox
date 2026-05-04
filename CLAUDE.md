# claude-code-yolo

Containerized Claude Code environments using Docker with sandbox command proxying.

## sandbox setup

The sandbox binary **must be built with musl as a static binary**. `claude-yolo-automate` bind-mounts the host binary into the container as the proxy shim, so a glibc-linked binary fails to load inside any base image whose libc is older than the host's (`libc.so.6: version GLIBC_2.39 not found`). Static musl removes the dependency entirely.

The default target is locked to `x86_64-unknown-linux-musl` via `ultra-sandbox/sandbox-rs/.cargo/config.toml` — `cargo build` without `--target` already builds musl. Do not bypass this config with an explicit `--target=*-gnu`.

```bash
# One-time: install the musl target
rustup target add x86_64-unknown-linux-musl

# Build (musl by default) and install
cd ultra-sandbox/sandbox-rs
cargo build --release
install -m 755 target/x86_64-unknown-linux-musl/release/sandbox ~/.local/bin/sandbox
```

For ARM64 hosts, override once and install from the matching path:
```bash
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
install -m 755 target/aarch64-unknown-linux-musl/release/sandbox ~/.local/bin/sandbox
```

Verify the result is static (no dynamic libc):
```bash
ldd ~/.local/bin/sandbox  # expect: "statically linked"
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

Block specific subcommands per mapped command:

```bash
sandbox policy deny podman rm
sandbox policy deny podman system prune
sandbox policy allow kubectl get
sandbox policy list
```

Then launch the container:

```bash
claude-yolo-automate                   # Claude Code in any project
bash ultra-sandbox/ultra-sandbox.sh    # Generic dev shell, no Claude
```
