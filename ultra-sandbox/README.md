# ultra-sandbox

A lightweight proxy tool for transparently running host commands from inside a container. Routes command execution requests from the container to the host via a Unix socket, with full stdin/stdout/stderr passthrough, TTY support, window resize, and signal forwarding.

---

## Architecture

```
Host                                Container
┌──────────────────────────┐       ┌──────────────────────────┐
│ sandbox daemon           │       │ PATH=/ultra_sandbox:...  │
│  .ultra_sandbox/         │◄─────►│                          │
│  daemon.sock (unix sock) │ frame │ /ultra_sandbox/docker    │
│                          │ proto │  └─ shim: sandbox run    │
│  executes: docker build .│       │         docker "$@"      │
└──────────────────────────┘       └──────────────────────────┘
          ▲
          │ -v .ultra_sandbox:/ultra_sandbox
          │ -e SANDBOX_SOCKET=/ultra_sandbox/daemon.sock
```

The container uses `--network=host`. The `.ultra_sandbox/` directory is mounted into the container via volume, containing the sandbox binary and the daemon socket.

---

## File Structure

```
ultra-sandbox/
├── sandbox/              # Go source
│   ├── main.go
│   └── go.mod
├── .ultra_sandbox/       # Runtime directory (auto-created)
│   ├── sandbox           # Compiled binary
│   ├── daemon.sock       # Unix socket (while daemon is running)
│   ├── docker            # Shim created by: sandbox map docker
│   └── ...               # Other mapped commands
├── ultra-sandbox.sh      # Generic container launch script
├── claude_code_base.Dockerfile
└── ultra-sandbox.Dockerfile
```

---

## Installation

```bash
cd sandbox
go build -o ~/.local/bin/sandbox .
```

Ensure `~/.local/bin` is in your `PATH`.

---

## Usage

### 1. Start the daemon (on host)

```bash
# Foreground
.ultra_sandbox/sandbox daemon

# Background
.ultra_sandbox/sandbox daemon &

# Custom socket path
.ultra_sandbox/sandbox daemon --socket /tmp/my.sock
```

### 2. Map commands

```bash
# Create a shim script in .ultra_sandbox/
.ultra_sandbox/sandbox map docker
.ultra_sandbox/sandbox map adb

# Remove a shim
.ultra_sandbox/sandbox map docker --remove
```

Shim content (e.g. for docker):
```sh
#!/bin/sh
exec sandbox run docker "$@"
```

### 3. Start a container

```bash
# ultra-sandbox.sh automatically mounts .ultra_sandbox/ and sets PATH + SANDBOX_SOCKET
bash ultra-sandbox/ultra-sandbox.sh
```

### 4. Use inside the container

```bash
# Works exactly like on the host
docker ps
docker build -t myimage .
docker run -it alpine sh   # full TTY interaction
adb devices
```

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SANDBOX_SOCKET` | `.ultra_sandbox/daemon.sock` | Path to the daemon Unix socket |

---

## Frame Protocol

```
[1B type][2B length big-endian][data...]
```

| Direction | Type | Meaning |
|-----------|------|---------|
| Client→Server | 0x01 EXEC | JSON: cmd/args/cwd/tty/rows/cols |
| Client→Server | 0x02 STDIN | Raw bytes |
| Client→Server | 0x03 RESIZE | 4B: rows(u16) cols(u16) |
| Client→Server | 0x04 SIGNAL | 1B signal number |
| Client→Server | 0x05 EOF | Empty |
| Server→Client | 0x11 STDOUT | Raw bytes |
| Server→Client | 0x12 STDERR | Raw bytes |
| Server→Client | 0x13 EXIT | 4B int32 exit code |
