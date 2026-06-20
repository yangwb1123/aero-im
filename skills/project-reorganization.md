# Skill: Project Reorganization（项目结构治理 · Cargo workspace 版）

> 非功能性结构重构：在**不改变任何业务行为**的前提下，遏制根目录膨胀、保持
> crate/模块边界清晰。本 skill 是 **Rust workspace 适配版**——不要套用
> Node/NestJS 的 `src/{domain}/{controller,application,domain,infrastructure,dto}`
> 布局；在 Cargo 工程里，**crate 就是 feature-first 的单位**。

## 何时触发

满足任一即应在加新功能前先执行本 skill（见 `AGENTS.md §4.2`）：

- 根目录常规文件 > 16（见下方 allowlist；超出的必须是真·可迁移项，不是 Rust 强制文件）；
- 根目录出现散落脚本/临时文件（`*.py` / `test_*` / `fix_*` / `demo_*` / `temp_*` / `run_*.sh` 等一次性产物）；
- 某 crate 的 `src/` 顶层文件 > 12 个且无子模块分组；
- 出现 God file（违反 `scripts/file-size-check.sh` 的 HARD 线）或职责混杂的 `utils.rs`/`helpers.rs` 巨集。

## 根目录 allowlist（Cargo workspace）

**强制在根、不可迁移**：`Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`。

**约定在根（允许）**：`README.md`、`AGENTS.md`、`HARNESS.md`、`BOOTSTRAP.md`、
`Makefile`、`deny.toml`（cargo-deny 约定根）、`docker-compose.yml`、`Dockerfile`、
`.gitignore`、`.env.example`、`config.example.toml`、CI 配置。

**其余一律迁移**到对应目录：`config/`（运行期配置）、`scripts/`（脚本）、
`docs/`（文档/ADR/spec）、`migrations/`、`monitoring/`。

> 反例（禁止出现在根）：`test.rs` `util.rs` `fix.rs` `demo.rs` `temp.rs` `scratch.py`。

## 结构原则（Rust idiom，勿照搬 Node）

1. **feature-first = crate**。每个 bounded domain 一个 crate（`crates/aero-*`），
   各带独立 `Cargo.toml + src/`。新业务领域优先开新 crate，而非往 `aero-server` 堆。
2. **crate 内按职责分模块**，不是按 Node 分层：用
   `repo`/`service`/`route`/`handler`/`model`，子模块走目录（`message/{repo,search,retention}.rs`），
   `mod.rs` 只做 `pub mod` + `pub use` 收口。**不要**造 `controller/application/domain/infrastructure/dto` 目录。
3. **依赖自下而上、勿成环**（`AGENTS.md §1` crate 地图 + `scripts/dependency-check.sh`）。
4. **入口最少化**：二进制入口在 `crates/aero-server/src/bin/`（`main.rs` + `bin/*`），
   启动逻辑拆到 `bin/boot/`。不在 crate root 堆裸 `fn main` 旁路。

## 尺寸阈值（**对齐本仓 harness，不是通用的 500**）

以 `scripts/file-size-check.sh` 为准，**不要引入冲突的第二套数字**：

| 类型 | WARN | HARD（禁止修改，先拆） | 豁免 |
|---|---|---|---|
| Rust `*.rs` | 800 行 | 1200 行 | `routes.rs` ≤ 3000 |
| Web `*.js` | 600 行 | 1000 行 | — |
| 函数 | — | 50 行（`scripts/complexity-check.sh`） | — |
| 圈复杂度 | — | 10 | — |

> **为什么不是 500**：Rust 比 Node 啰嗦，一个内聚的 `impl` 块合理地会偏长；把上限
> 压到 500 会逼出**劣质机械拆分**（为降行数而切碎、甚至删功能），反而制造更多 bug。
> 杠杆不在更小的数字，在**拆分质量 + 可达性 + 真实状态**。

## 执行流程

### Phase 1 — 分析（只读，必须等确认）

1. 清点根目录常规文件，对照 allowlist 标出真·可迁移项（排除 Rust 强制文件）。
2. 扫描散落脚本/临时产物、God file、职责混杂模块。
3. **先判断「本项目是否真有这个病」**——若根目录已自律、已是 crate-based feature-first，
   **结论可以是「无需迁移」**。资深工程师的第一步是质疑约束是否适配，而非盲目执行。
4. 输出问题清单 + 建议结构 + **风险评估** → **停下等确认**。

### Phase 2 — 迁移（仅在 Phase 1 确认后）

- `git mv` 移动文件（保留历史）；更新 `mod`/`use`/`pub use` 与 `routes::build` 合并链；
- 删除确认废弃的一次性产物；
- **反 Goodhart 红线**：结构重构**不得净删功能**——迁移前后该区域的 public 符号数 /
  导出项 / 测试数**不得下降**（用 `git diff` 核对 API surface）。拆分只能搬运，不能蒸发。

### Phase 3 — 验证

```bash
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets   # 不得新增警告
cargo test --workspace --lib
bash scripts/file-size-check.sh           # 0 HARD
bash scripts/dependency-check.sh          # 0 反向依赖
# 涉及 web/ 时：见 HARNESS.md 的 JS lint（若尚未接入，结构迁移前先补，否则前端破损无人接）
```

确保：全部测试通过、`cargo build --bin aero-server` 成功、对外行为零变化。

## 暂停条件（强制 STOP，不得盲目继续）

发现以下任一，**停止并提方案**，不要硬推：

- **循环依赖** 或 公共 crate 过度耦合；
- **工作树正处在另一次重构中途**（`git status` 有大量在途删除/未跟踪新文件）——
  **禁止在未完成的结构迁移上叠加第二次迁移**；先收口在途的，再开新的；
- 构建/测试当前是红的（先修绿，迁移会掩盖真因）；
- 约束彼此矛盾（例如「0 HARD 退出」与「禁改某 crate」同时成立）——这是约束集自身的 bug，先解矛盾。

## 输出格式

### Refactoring Summary
新增目录 / 修改文件 / 移动文件（`git mv`）/ 删除文件。

### Architecture Impact
解决的问题 / 仍存在的问题 / 建议后续优化。
