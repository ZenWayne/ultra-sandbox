# Ultra-sandbox

> 在容器内运行 Claude Code（或任意 CLI 工具）并带上 `--dangerously-skip-permissions`，同时透明地使用**宿主机**上的 `docker`、`flutter`、`adb` 等工具。

**语言**: [English](README.md) | 简体中文

Ultra-sandbox 是一套轻量的命令代理系统：宿主机上运行一个微型守护进程，通过 Unix socket 上的帧协议通信，再配合一个即插即用的 shim,让容器内的命令实际在宿主机上执行——无需 Docker-in-Docker、SSH 隧道或特权容器。

---
## 演示
<img src="demo.gif" width="100%" />

## 架构

```
┌────────────────────────────── 宿主机 ──────────────────────────────┐
│                                                                    │
│    真实的 docker / flutter / adb / …                               │
│               ▲                                                    │
│               │ fork + exec                                        │
│               │                                                    │
│       ┌───────┴─────────┐                                          │
│       │ sandbox daemon  │   (./sandbox daemon)                     │
│       └───────┬─────────┘                                          │
│               │                                                    │
│               │ Unix socket 上的帧协议                             │
│               │ [1B type][2B len BE][payload]                      │
│               │ EXEC / STDIN / STDOUT / STDERR /                   │
│               │ RESIZE / SIGNAL / EXIT                             │
│               │                                                    │
│     .ultra_sandbox/                                                │
│       ├─ daemon.sock        ◄─── 通过 bind-mount 挂入容器          │
│       └─ bin/                                                      │
│          ├─ docker   ─► #!/bin/sh exec sandbox run docker "$@"     │
│          ├─ flutter  ─► #!/bin/sh exec sandbox run flutter "$@"    │
│          └─ adb      ─► #!/bin/sh exec sandbox run adb "$@"        │
│                                                                    │
└──────────┬─────────────────────────────────────────────────────────┘
           │  -v .ultra_sandbox:/ultra_sandbox
           │  -v ~/.local/bin/sandbox:/usr/local/bin/sandbox:ro
           │  -e PATH=/ultra_sandbox/bin:$PATH
           │  -e SANDBOX_DIR=/ultra_sandbox
           ▼
┌──────────────────────────── 容器 ────────────────────────────────┐
│                                                                   │
│   Claude Code (--dangerously-skip-permissions)                    │
│        │                                                          │
│        │ 调用 `docker build .`                                    │
│        ▼                                                          │
│   /ultra_sandbox/bin/docker   (shim)                              │
│        │                                                          │
│        │ exec sandbox run docker build .                          │
│        ▼                                                          │
│   /usr/local/bin/sandbox  (客户端)                                │
│        │                                                          │
│        │ 连接 /ultra_sandbox/daemon.sock                          │
│        │ 通过帧协议通信                                           │
│        ▼                                                          │
│   → 在宿主机上执行，输出回流（完整 TTY、信号）                    │
│                                                                   │
└───────────────────────────────────────────────────────────────────┘
```

**核心思路。** 容器本身从不运行 `docker`,而是运行一个仅 3 行的 shell shim,把命令交给 `sandbox` 客户端,再由它通过 Unix socket 转发给宿主机上的守护进程。stdin/stdout/stderr/TTY-resize/信号都通过同一个 socket 上的 3 字节帧协议多路复用,因此在容器内执行 `docker run -it alpine sh` 依然能得到一个在**宿主机**上真正交互的 shell。

---

## 快速开始

### 前置条件

- 宿主机上已安装 **Docker**。macOS/Windows: 安装 [Docker Desktop](https://www.docker.com/products/docker-desktop/)(它会自动管理 Linux 虚拟机——`--network=host` 指向的是那个虚拟机)。Linux: 用发行版自带的 `docker` 包即可。
- 把 `~/.local/bin`(Linux/macOS)或 `%USERPROFILE%\.local\bin`(Windows)加入 `PATH`。

### 1. 运行安装脚本

克隆仓库后执行对应平台的安装脚本——它会一键完成所有事情:拉取 `sandbox` release、构建 `claude_code_base` 镜像、把 `claude-yolo-automate` 放到 `$PATH`。

**Linux / macOS / WSL2:**
```bash
git clone https://github.com/ZenWayne/ultra-sandbox.git
cd ultra-sandbox
./install.sh
```

**Windows(原生,PowerShell):**
```powershell
git clone https://github.com/ZenWayne/ultra-sandbox.git
cd ultra-sandbox
.\install.ps1
```

> `claude-yolo-automate` 是 bash 脚本。在原生 Windows 上要通过 Git Bash、MSYS2 或 WSL2 运行——或者直接在 WSL2 里用 `install.sh` 获得纯 bash 流程。

**环境变量(两个安装脚本通用):**

| 变量 | 默认值 | 用途 |
|---|---|---|
| `INSTALL_DIR` | `~/.local/bin` / `%USERPROFILE%\.local\bin` | `sandbox` 和 launcher 的安装位置 |
| `REPO` | `ZenWayne/ultra-sandbox` | GitHub 仓库 |
| `RELEASE_TAG` | `latest` | Release 标签 |
| `IMAGE_TAG` | `claude_code_base` | Docker 镜像名 |
| `SKIP_SANDBOX` | — | `=1` 跳过二进制下载 |
| `SKIP_IMAGE` | — | `=1` 跳过镜像构建 |
| `SKIP_LAUNCHER` | — | `=1` 跳过 launcher 安装 |

**从源码构建**(Intel Mac、其他架构,或不想用预编译版本):

```bash
cd ultra-sandbox/sandbox-rs
cargo build --release
install -m 755 target/release/sandbox ~/.local/bin/sandbox   # Linux/macOS
# Windows: Copy-Item target\release\sandbox.exe $env:USERPROFILE\.local\bin\sandbox.exe
```

然后带 `SKIP_SANDBOX=1` 重跑安装脚本,只做镜像构建和 launcher 安装。

### 2. 在任意项目中启动 Claude Code

```bash
cd /path/to/your/project

# 将 `python` 从容器代理到宿主机,然后启动 Claude Code。
SANDBOX_MAP_PROCESSES="python" claude-yolo-automate
```

完成。进入容器后:

```bash
> 你能帮我构建这个仓库里的 Docker 镜像吗?

# Claude 执行:
docker build -t myapp .         # ← 实际在宿主机上运行
```

- 守护进程会在启动脚本第一次运行时自动拉起。
- `.ultra_sandbox/daemon.sock` 默认位于 `$HOME/.ultra_sandbox/`(可用 `SANDBOX_DIR` 覆盖)。
- 需要代理更多命令时,扩展环境变量即可:`SANDBOX_MAP_PROCESSES="docker adb flutter"`。

---

## 为什么选择 Ultra-sandbox

| 问题 | 传统方案 | Ultra-sandbox 方案 |
|---|---|---|
| 容器内需要 `docker build` | Docker-in-Docker、特权容器 | `sandbox map docker` → 透明代理 |
| 物理设备需要宿主机 ADB 服务 | 挂载 `/dev/bus/usb`、`--privileged` | `sandbox map adb`、`--network=host` |
| Flutter 构建污染宿主机 `build/` 目录 | 手动清理 | 在 `build/`、`.dart_tool/` 上叠加命名卷 |
| Claude Code 会话需持久化 | 每次启动重新登录 | 读写挂载 `~/.claude` + `~/.claude.json` |
| 用 SSH 密钥推送 git,但不想让容器碰密钥 | 把密钥复制进镜像 | `~/.ssh` **只读**挂载 |
| 文件属主不一致(容器里是 `root`,宿主机是用户) | 每次运行后 `chown -R` | `--userns=keep-id` —— 保留宿主机 UID/GID |
| 宿主机的代理配置(HTTP_PROXY 等) | 带新代理重建镜像 | `--network=host` + `replace_proxy()` 助手 |

---

## 启动脚本

仓库提供两个启动脚本:

| 脚本 | 镜像 | 映射的命令 | 适用场景 |
|---|---|---|---|
| `claude-yolo-automate` | `claude_code_base` | `$SANDBOX_MAP_PROCESSES`(通过环境变量) | **任意项目**——通用、可配置 |
| `ultra-sandbox/ultra-sandbox.sh` | `claude_code_base` | —(手动 `sandbox map`) | 纯开发 shell,不带 Claude Code |

两者共享同一套设计:

- `--userns=keep-id` —— 容器内 UID/GID = 宿主机 UID/GID
- `--network=host` —— 复用宿主机的代理、ADB server、docker daemon
- 将 `$WORK_DIR:$WORK_DIR` 挂载在**同路径**(Claude 在容器内外看到的项目绝对路径一致)
- 读写挂载 `~/.claude`、`~/.claude.json` —— 持久化会话与认证
- **只读**挂载 `~/.ssh` —— git over SSH 可用,Claude 无法泄露密钥
- 代理改写助手:`127.0.0.1:10809` → `host.docker.internal:10809`

### 示例:用通用启动器加载自定义命令集

```bash
SANDBOX_MAP_PROCESSES="docker adb kubectl" ./claude-yolo-automate
```

脚本会:
1. 若 sandbox daemon 未运行则自动启动。
2. 在 `$SANDBOX_DIR/bin/{docker,adb,kubectl}` 中生成 shim。
3. 启动 `claude_code_base`,挂载 `.ultra_sandbox/` 并设置 `PATH`。

---

## 模式:代理路径会变化的 MCP stdio 服务

部分应用以 AppImage 形式分发,启动时会把自己挂到一个随机路径上——例如 [Pencil](https://getpencil.dev/) 每次出现在 `/tmp/.mount_Pencil<随机串>/`,其自带的 MCP server 位于 `/tmp/.mount_Pencil<随机串>/resources/app.asar.unpacked/out/mcp-server-linux-x64`。把这个路径硬编码进 `~/.claude.json` 意味着每次重启都要手改,而且在容器内这个 FUSE 挂载根本不可访问(rootless 容器运行时无法 bind-mount FUSE 源)。

仓库提供了一个小巧的动态解析包装脚本 `update-pencil-mcp`:在 exec 时扫描 `/tmp/.mount_Pencil*/resources/app.asar.unpacked/out/mcp-server-linux-x64`,然后 exec 到最新的那个。让 Claude Code 指向这个稳定的命令名,由包装脚本跟随活动挂载点即可。

### 配置步骤

1. **把包装脚本安装到宿主机。**
   ```bash
   install -m 755 update-pencil-mcp ~/.local/bin/update-pencil-mcp
   ```

2. **修改 `~/.claude.json`** —— 把 Pencil 的 MCP 条目从绝对 FUSE 路径改为稳定命令名:
   ```json
   "pencil": {
     "type": "stdio",
     "command": "update-pencil-mcp",
     "args": ["--app", "desktop"]
   }
   ```

3. **把它代理进容器** —— 将 `update-pencil-mcp` 加入 `SANDBOX_MAP_PROCESSES`:
   ```bash
   SANDBOX_MAP_PROCESSES="update-pencil-mcp docker" ./claude-yolo-automate
   ```

在容器内,Claude 执行 `update-pencil-mcp` → shim → 宿主机 daemon → 宿主机 wrapper → 当前的 MCP 二进制。stdio 通过帧协议端到端转发,MCP 握手能正常完成,**无需**对 `/tmp/.mount_Pencil*` 做任何 bind-mount。

该模式适用于任何二进制路径会在不同运行之间发生变化的 stdio MCP(或普通 CLI):在 `~/.local/bin/` 放一个小解析脚本,把配置指向稳定名称,再把它加到 `SANDBOX_MAP_PROCESSES` 即可。

---

## `sandbox` 二进制

```
sandbox daemon [--socket PATH]      启动宿主机守护进程
sandbox run <cmd> [args...]         通过守护进程执行 <cmd>
sandbox map <cmd> [--remove]        在 $SANDBOX_DIR/bin/ 创建/移除 shim
```

**环境变量:** `SANDBOX_DIR`(默认为 `.ultra_sandbox`)同时控制 socket 路径(`$SANDBOX_DIR/daemon.sock`)和 shim 目录(`$SANDBOX_DIR/bin/`)。

**跨平台:** 实现(`ultra-sandbox/sandbox-rs/`)面向 Linux、macOS(x86_64 + arm64)和 Windows(通过 `uds_windows` + ctrlc)。四个平台的 CI 构建在 `.github/workflows/build.yml` 中定义。

---

## 仓库结构

```
ultra-sandbox/                          # 仓库根目录
├── .github/workflows/build.yml         # 跨平台 CI (linux/mac/win)
├── README.md                           # 英文 README
├── README.zh-CN.md                     # 你正在看这里
├── CLAUDE.md                           # 面向 Claude Code 的构建/运行说明
│
├── claude-yolo-automate                # 通用启动脚本(环境变量驱动映射)
├── install.sh                          # 一键安装脚本(Linux/macOS/WSL2)
├── install.ps1                         # 一键安装脚本(Windows / PowerShell)
├── update-pencil-mcp                   # Pencil MCP 路径的动态解析器
│
└── ultra-sandbox/
    ├── README.md                       # 更深入的协议文档
    ├── ultra-sandbox.sh                # 通用 dev-shell 启动脚本(不带 Claude)
    ├── claude-yolo-base-docker.sh      # 纯 Claude Code 启动脚本(不带 sandbox)
    ├── claude_code_base.Dockerfile     # debian + Node.js + Claude Code
    ├── ultra-sandbox.Dockerfile        # debian + 通用开发工具
    │
    └── sandbox-rs/                     # Rust sandbox(跨平台,CI 构建)
        ├── Cargo.toml
        ├── Cargo.lock
        └── src/main.rs
```

---

## 帧协议(简要参考)

```
frame = | type: u8 | length: u16 BE | payload: [u8; length] |
```

| 方向 | 类型 | Payload |
|---|---|---|
| client → server | `0x01` EXEC | JSON `{cmd, args, cwd, tty, rows, cols}` |
| client → server | `0x02` STDIN | 原始字节 |
| client → server | `0x03` RESIZE | `rows: u16 BE, cols: u16 BE` |
| client → server | `0x04` SIGNAL | `sig: u8` |
| client → server | `0x05` EOF | 空 |
| server → client | `0x11` STDOUT | 原始字节 |
| server → client | `0x12` STDERR | 原始字节 |
| server → client | `0x13` EXIT | `code: i32 BE` |

完整细节参见 `ultra-sandbox/README.md` 与 `ultra-sandbox/sandbox-rs/src/main.rs`。

---

## 疑难排查

**`sandbox: cannot connect to daemon at ...`**
守护进程没在运行。手动启动:`sandbox daemon &`。启动脚本会自动执行这一步。

**`Error: 'sandbox' not found in PATH`**
你没有把二进制安装到 `~/.local/bin/sandbox`。见快速开始第 1 步。

**容器内的命令没有解析到 shim**
检查容器内的 `PATH`,必须以 `/ultra_sandbox/bin` 开头。用 `echo $PATH` 确认。

**容器内无法访问代理**
如果你的本地 HTTP 代理在 `127.0.0.1:10809`,启动脚本会自动改写成 `host.docker.internal:10809`。其他端口请编辑启动脚本里的 `replace_proxy()`。

**Claude 会话未持久化**
启动前确保宿主机上 `~/.claude` 和 `~/.claude.json` 已存在。这两个路径是读写挂载,Claude 写入的内容会在容器重启后保留。

**Pencil MCP 报错 "no such file" 或 `WebSocket not connected`**
Pencil 的 MCP 二进制位于 `/tmp/.mount_Pencil<随机串>/`,随机后缀每次启动都会变。使用自带的 `update-pencil-mcp` 并把它加入 `SANDBOX_MAP_PROCESSES`——参见上方*代理路径会变化的 MCP stdio 服务*。

---

## 许可证

见 `LICENSE`。
