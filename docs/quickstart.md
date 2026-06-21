# Parrot Quick Start

## Build

```powershell
cargo build
```

## Run

需要 **两个终端**，先启动 daemon 再启动 CLI。

### Terminal 1 — Daemon（后端）

```powershell
$env:ANTHROPIC_API_KEY = "your-api-key"
.\target\debug\parrotd.exe
```

### Terminal 2 — CLI（客户端）

```powershell
.\target\debug\parrot.exe
```

## CLI 用法

```powershell
# 交互模式（默认，支持历史记录 / 方向键）
.\target\debug\parrot.exe

# 非交互模式（一次性消息）
.\target\debug\parrot.exe --message "你的问题"

# 连接远程 daemon
.\target\debug\parrot.exe --connect ws://remote-host:9876

# 指定 token 文件路径
.\target\debug\parrot.exe --token-file ./my-token
```

## 配置文件

项目根目录下的 `parrot.toml`，支持多 provider、工具权限、会话管理等配置。
