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
| `aero-cli check` | 并行基础检查 + 原生门禁 | `cargo check` + `cargo test --workspace --lib` + `cargo clippy` 并行；随后原生 filesize/deps/workspace/todos/metadata 检查 |
| `aero-cli test` | 运行测试 | `cargo test --workspace --lib` |
| `aero-cli gate list` | 列出门禁 | 列出下方所有 gate 名称 |
| `aero-cli gate filesize` | 文件尺寸 | `scripts/file-size-check.sh`（固定脚本阈值） |
| `aero-cli gate truth` | 死代码检测 | `scripts/truth-check.sh` |
| `aero-cli gate web` | 前端完整性 | `scripts/web-check.sh` |
| `aero-cli gate deps` | 依赖方向 | `scripts/dependency-check.sh` |
| `aero-cli gate complexity` | 长函数近似检查 | `scripts/complexity-check.sh`（固定 >50 行，告警式、非实际圈复杂度） |
| `aero-cli gate format` | 增量 rustfmt；基线整洁/新增文件不得引入格式债 | `scripts/fmt-check.sh`（历史格式债仅告警） |
| `aero-cli gate format-tests` | 增量 rustfmt 门禁正反例回归 | `scripts/test-fmt-check.sh`（无数据库、无网络） |
| `aero-cli gate migrations` | 历史迁移不可变与新增序号检查 | `scripts/migration-immutability-check.sh` |
| `aero-cli gate migration-tests` | 迁移守卫正反例回归 | `scripts/test-migration-immutability-check.sh`（无数据库、无网络） |
| `aero-cli gate filesize-native` | 文件尺寸（原生 Rust） | 递归扫描，使用 engineering.toml 的 filesize 阈值 |
| `aero-cli gate deps-native` | 依赖方向（原生 Rust） | 解析 Cargo.toml 的生产/构建依赖，校验 ALLOWED_DEPS（dev-dependencies 仅供测试，不计入生产方向图） |
| `aero-cli gate workspace-members` | workspace 成员完整性 | 校验 Cargo.toml 声明的 crate 目录均存在 |
| `aero-cli gate todos` | TODO/FIXME/HACK 提醒 | 扫描 crate Rust 注释，告警式、非失败门 |
| `aero-cli gate metadata` | crate 元数据完整性 | 校验每个 workspace crate 使用 workspace 的 version/edition/license |
| `aero-cli gate readme` | crate 文档完整性 | 每个 workspace crate 必须有首个非空行以 crate 名开头的 README H1；缺失或标题不匹配直接失败，不检查占位正文 |
| `aero-cli gate b5` | B5 集成门 | `scripts/test-integration.sh`（需集成环境） |
| `aero-cli gate all` | 标准门禁汇总 | 并行运行 7 个 shell gate（filesize/truth/web/deps/complexity/format/migrations），随后运行 6 个原生 gate（filesize/deps/workspace/todos/metadata/readme）；不包含 format-tests、migration-tests 或 b5 |

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

### 阈值语义

文件尺寸比较使用严格的 `>`：正好达到阈值不触发该级别；例如 Rust
801 行开始告警、1201 行开始硬失败，JS 601 行开始告警、1001 行开始硬失败。
Rust 在 1200 行时仍是告警，`routes.rs` 只有超过 3000 行才硬失败。告警不会
让 shell gate 或命令退出码变为失败，硬失败退出码为 1。

### `engineering.toml`

`aero-cli`/`aero-eng` 启动时会读取此文件。`[filesize]` 由
`gate filesize-native`、`check` 和 `gate all` 的原生 filesize 检查使用；
`gate filesize` 使用 `scripts/file-size-check.sh` 内的固定默认值，因此自定义
配置不会改变 shell gate。当前 `[complexity]` 和 `[check]` 只会被解析，尚未
改变 gate 行为：`gate complexity` 仍是固定的 >50 行近似告警，`check` 始终运行
cargo check、`cargo test --workspace --lib` 和 clippy。

```toml
[filesize]
rust_warn = 800      # 严格按 > 比较；超过此行数告警
rust_hard = 1200     # 严格按 > 比较；超过此行数硬失败
routes_hard = 3000   # routes.rs 独立硬阈值
js_warn = 600        # JS 严格按 > 比较的告警线
js_hard = 1000       # JS 严格按 > 比较的硬失败线

# Parsed for compatibility; gate complexity currently uses scripts/ constants.
[complexity]
warn = 12
hard = 20

# Parsed for compatibility; check currently always runs all three cargo commands.
[check]
clippy = true
full_test = true
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
make check                # cargo check --workspace --all-targets
make gate                 # 通过 aero-cli 运行 gate all
make doctor               # 环境诊断

# CI 管线
make ci                   # aero-cli check + gate all
```
