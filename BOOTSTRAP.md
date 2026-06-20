# BOOTSTRAP.md — Agent 入口文档

> 如果你是第一次进入这个仓库的新 Agent，先读这个文件。它告诉你"先看什么"。

## 1. 仓库是什么

**Aero IM** 是一个 AI-Native 即时通讯 + 直播平台。

- Rust 2021 / MSRV 1.80
- 15 个 crate，363 个 .rs 源文件，135 个 SQL 迁移
- 技术栈：tokio + axum 0.7 + sqlx 0.8 + fred 9 + async-nats 0.36

## 2. 阅读顺序

1. **先读** `AGENTS.md` — 架构骨架、智能体清单、硬性工程规则
2. **然后** `HARNESS.md` — 自动检查与拒绝策略（不通过不能提交）
3. **然后** `README.md` — 功能矩阵和 API 速查
4. **然后** `docs/ROADMAP.md` — 战略方向
5. **然后** `docs/sprint/TODO.md` — 当前待办
6. **然后** `docs/sprint/CURRENT_SPRINT.md` — 当前 Sprint 的目标和限制
7. **最后** `skills/` — 遇到需要重构/组织模块/保持架构时查阅

## 3. 硬性约束（违反即 STOP）

| 规则 | 数值 | 文件引用 |
|---|---|---|
| 单 .rs 文件 | ≤ 800 行（警告），≤ 1200 行（禁止修改） | `AGENTS.md §3.3` |
| routes.rs 豁免 | ≤ 3000 行 | `AGENTS.md §3.3` |
| 单函数 | ≤ 50 行 | `AGENTS.md §3.3` |
| 圈复杂度 | ≤ 12 | `AGENTS.md §3.3` |
| `unsafe_code` | forbid | `AGENTS.md §3.2` |
| clippy warning | 不得新增 | `AGENTS.md §3.2` |
| 依赖方向 | 禁止循环依赖、禁止向上依赖 | `skills/clean-architecture.md` |
| AI 无 key 降级 | HashEmbedder + 启发式 | `AGENTS.md §3.2` |

## 4. 提交前必须做的事

```bash
make check-harness     # cargo check + 文件尺寸 + 函数复杂度 + 依赖方向
```

不通过 → 停掉新功能 → 先去 `docs/sprint/TODO.md` P0_REFACTOR 列表里选一个重构。

通过但仍有 WARNING → 新增功能前先拆分 top-1 超限文件。

### 完整提交门控

```bash
make check-full       # check-harness + clippy + test
```

**第一次修改后，后续每次 edit 自动运行：**

```bash
# .claude/settings.local.json 已配置 hooks.post_edit = ["make check-harness"]
```

如果 Agent 不支持 hooks，每次 edit 后**必须人工调用** `make check-harness`。

## 5. 工程体系总览

Agent Engineering Operating System for Aero IM：

```
┌─ BOOTSTRAP.md ────────────────────────────────────────┐
│  入口：告诉新 Agent 先读什么、硬约束是什么              │
└────────────────────────────────────────────────────────┘
         │
         ▼
┌─ AGENTS.md ───────────────────────────────────────────┐
│  规则：架构骨架、crate 地图、智能体清单、硬约束、陷阱   │
│  §3.3 — 文件 ≤ 800/1200 行 · 函数 ≤ 50 行 · match ≤ 8 │
└────────────────────────────────────────────────────────┘
         │
         ▼
┌─ docs/sprint/CURRENT_SPRINT.md ──────────────────────┐
│  目标：P0_REFACTOR 优先 · 禁止新功能直到债务清除        │
└────────────────────────────────────────────────────────┘
         │
         ▼
┌─ SKILLS/ ────────────────────────────────────────────┐
│  能力：遇到大文件、依赖违规、模块组织时查阅              │
│  ├ refactor-large-file.md — 拆文件的分步指南           │
│  ├ modularization.md — Rust 模块组织规范               │
│  ├ clean-architecture.md — crate 依赖规则+决策树       │
│  ├ feature-development.md — 加功能的标准化流程      │
│  └ testing.md — 测试策略与模式                         │
└────────────────────────────────────────────────────────┘
         │
         ▼
┌─ HARNESS.md ─────────────────────────────────────────┐
│  门控：拒绝策略 · 提交格式 · hooks 配置                │
└────────────────────────────────────────────────────────┘
         │
         ▼
┌─ scripts/ ───────────────────────────────────────────┐
│  file-size-check.sh     → 尺寸强制 (800/1200)         │
│  complexity-check.sh    → 函数长度强制 (50)            │
│  dependency-check.sh    → 依赖方向合规                 │
│  crate-dependencies.sh  → 依赖可视化                   │
└────────────────────────────────────────────────────────┘
         │
         ▼
┌─ Makefile ───────────────────────────────────────────┐
│  check-harness = check + size + complexity + deps    │
│  check-full    = check-harness + clippy + test       │
│  check-security  → cargo deny                        │
│  check-coverage  → cargo llvm-cov                    │
└────────────────────────────────────────────────────────┘
         │
         ▼
┌─ .claude/settings.local.json ────────────────────────┐
│  hooks.post_edit = ["make check-harness"]            │
└────────────────────────────────────────────────────────┘
```

## 6. 我该从哪开始

**每次启动时，运行任务规划器：**

```bash
bash scripts/task-planner.sh
```

这会自动判断当前状态，输出下一步行动指令。

| 输出 | 意义 | 下一步 |
|---|---|---|
| `Phase 1 — 代码重构` | 有 HARD 违规 | `bash scripts/agent-start.sh` → 选 Step → 拆分 |
| `Phase 2 — 功能开发` | 全部通过 | `cat docs/sprint/CURRENT_SPRINT.md` → 找第一个 `[ ]` 任务 |
| `依赖修复` | 依赖方向违规 | `make check-deps-strict` 查看详情 |
| `编译修复` | 编译失败 | `cargo check --workspace` |

**也可以直接运行诊断查看详细信息：**

```bash
bash scripts/agent-start.sh
```

### 完整阅读顺序（首次进入）

1. `AGENTS.md` — 架构骨架、智能体清单、硬性工程规则
2. `HARNESS.md` — 自动检查与拒绝策略（不通过不能提交）
3. `README.md` — 功能矩阵和 API 速查
4. `docs/ROADMAP.md` — 战略方向
5. `docs/sprint/TODO.md` — 当前待办
6. `docs/sprint/CURRENT_SPRINT.md` — 当前 Sprint 的目标和限制
7. `skills/` — 遇到需要重构/组织模块/保持架构时查阅
