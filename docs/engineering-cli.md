# Aero Engineering CLI — 使用文档

`aero-cli` 是 Aero IM 的工程 CLI，统一纳管运维、工程门禁、测试调度、技能系统和环境诊断。

## 快速开始

```bash
# 构建
cargo build -p aero-server --bin aero-cli

# 查看所有命令
./target/debug/aero-cli help
```

## 命令参考

### 运维命令

| 命令 | 说明 | 依赖 |
|---|---|---|
| `aero-cli migrate` | 运行待处理的 DB 迁移 | PostgreSQL |
| `aero-cli health` | 检测 PG/Redis/NATS 连通性 | 各服务运行中 |
| `aero-cli ai-test` | 测试 AI 后端（embedder + LLM） | ANTHROPIC_API_KEY 可选 |
| `aero-cli streams` | 列出当前直播流 | PostgreSQL |
| `aero-cli ws-ping` | WebSocket 往返测试 | AERO_TOKEN + AERO_HOST |

### 工程命令

| 命令 | 说明 | 执行内容 |
|---|---|---|
| `aero-cli check` | 并行门禁 | `cargo check` + `test` + `clippy` 同时运行 |
| `aero-cli test` | 运行测试 | `cargo test --workspace --lib` |
| `aero-cli gate list` | 列出门禁 | — |
| `aero-cli gate filesize` | 文件尺寸 | `scripts/file-size-check.sh` |
| `aero-cli gate truth` | 死代码检测 | `scripts/truth-check.sh` |
| `aero-cli gate web` | 前端完整性 | `scripts/web-check.sh` |
| `aero-cli gate deps` | 依赖方向 | `scripts/dependency-check.sh` |
| `aero-cli gate deps-native` | 依赖方向（原生 Rust） | 解析 Cargo.toml，校验 ALLOWED_DEPS |
| `aero-cli gate filesize-native` | 文件尺寸（原生 Rust） | 递归扫描，使用 engineering.toml 阈值 |

### 测试调度

```bash
aero-cli smoke list          # 列出所有冒烟测试（约 40 个）
aero-cli smoke run <name>    # 运行指定冒烟测试
aero-cli integration         # 运行所有集成测试（需 PG）
```

### 技能系统

```bash
aero-cli skill list          # 列出技能
aero-cli skill view <name>   # 查看技能文档
aero-cli skill run <name>    # 运行可执行技能
```

### 诊断

```bash
aero-cli doctor              # 环境诊断：工具链、配置、Git、迁移
```

### 开发环境

```bash
aero-cli dev                 # 一键启动 Dev 环境（docker + migrate）
aero-cli dev --skip-services # 跳过 Docker 服务
```

### Shell 补全

```bash
aero-cli completion bash     # Bash 补全
aero-cli completion zsh      # Zsh 补全
aero-cli completion fish     # Fish 补全
```

## 配置

### `engineering.toml`

```toml
[filesize]
rust_warn = 800      # Rust 文件超过此行数报警
rust_hard = 1200     # Rust 文件超过此行数禁止
routes_hard = 3000   # routes.rs 独立阈值
js_warn = 1000       # JS 文件报警线

[check]
clippy = true        # check 命令是否运行 clippy
full_test = true     # check 命令是否运行完整测试
```

### 环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `AERO_HOST` | `ws://localhost:3030` | ws-ping 连接地址 |
| `AERO_TOKEN` | — | ws-ping 认证 JWT |

## 开发工作流

```bash
# 一次性设置
make install-hooks        # 安装 git pre-commit hook

# 日常开发
make dev                  # 启动环境
make check                # 运行工程门禁
make gate                 # 运行全部门禁
make doctor               # 环境诊断

# CI 管线
make ci                   # check + gate all
```
