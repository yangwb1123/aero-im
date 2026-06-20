# CURRENT_SPRINT.md — 当前 Sprint 目标与任务看板

> 这是 Agent 的任务看板。
> 每次 Agent 启动时读此文件决定下一步做什么。
> 完成一个任务后，将 `[ ]` 改为 `[x]` 并更新下方进度。

## Sprint 元信息

- **Sprint 编号**: S1（2026-06-15 ~ 2026-06-29）
- **状态**: 🔴 REFACTOR 阶段（14 个 HARD 违规待拆分）
- **切换至开发模式的条件**: `bash scripts/file-size-check.sh` 报告 0 HARD 违规

---

## 🚨 Phase 1: REFACTOR（优先于一切）

> 14 个文件 > 1200 行。必须拆分后才能开发新功能。
> 执行顺序见 REFACTOR_PLAN.md。每完成一个更新下方进度。

```
📊 重构进度:
   13/14 已完成
   █████████████░░ 93%

> 注意: 仅 `web/app.js` (2056 行) 仍需 JS 模块重构。所有 Rust 文件已清理完毕。
```

## Phase 2: 功能开发（REFACTOR 全部完成后自动激活）

### P0: 通知聚合摘要（ROADMAP 方向一）

目标：把 "99+" 修成 "3 条未读摘要"。

| 状态 | 任务 | 文件 | 预估 |
|---|---|---|---|
| [x] | 新增 notification_bundles 表（迁移） | `migrations/0143_notification_bundles.sql` | M |
| [x] | 通知插入点加延迟聚合逻辑 | `aero-storage/src/notification_bundle.rs` | M |
| [x] | push_bot 注入 collapse_key / apns-collapse-id | `aero-server/src/push_bot.rs` | S |
| [x] | WS 重连回放压缩 (?summarize=true) | `aero-server/src/ws/ws_impl.rs` | M |
| [ ] | web/app.js 超过 1000 行 (JS 模块拆分) | `web/app.js` | L |

### P0: 索引瘦身（ROADMAP 方向二）

目标：GIN/HNSW 索引加 WHERE deleted_at IS NULL。

| 状态 | 任务 | 文件 | 预估 |
|---|---|---|---|
| [x] | migration: GIN index partial | `migrations/0136_message_index_partial.sql` | S |
| [x] | migration: HNSW index partial | `migrations/0136_message_index_partial.sql` | S |
| [x] | 索引膨胀 Prometheus gauge | `aero-server/src/metrics.rs` + `aero-server/src/bin/aero-server.rs` | S |

### P1: 富文本编辑（ROADMAP 方向三）

目标：从纯文本到真正的富文本消息。

| 状态 | 任务 | 文件 | 预估 |
|---|---|---|---|
| [ ] | 服务端 Markdown 解析器 | `aero-common/src/markdown.rs`（已有 scaffold） | M |
| [x] | Span 合法性校验（嵌套深度） | `aero-im-core/src/validation.rs` | S |
| [ ] | Web 前端 span 渲染 | `web/render.js` | M |
| [ ] | 搜索高亮 ts_headline | `aero-storage/src/search_query.rs` | S |

### P1: 多级缓存（ROADMAP 方向四）

目标：WebSocket 热路径的 DB 往返从 3-5 次降到 0-1 次。

| 状态 | 任务 | 文件 | 预估 |
|---|---|---|---|
| [ ] | Participant profile 本地缓存 (DashMap + TTL) | `aero-server/src/state.rs` | M |
| [ ] | Room membership 批量预取 | `aero-server/src/hub.rs` | M |
| [ ] | 缓存命中率指标 | `aero-server/src/metrics.rs` | S |

---

## Phase 3: 能力扩展（Phase 2 全部完成后再考虑）

### P2: 开放平台（ROADMAP 方向五）

目标：Bot SDK + 应用目录。

| 状态 | 任务 | 文件 | 预估 |
|---|---|---|---|
| [ ] | Bot 注册与 token 管理 | `aero-server/src/pat.rs` | XL |
| [ ] | Bot API 端点 | `aero-server/src/routes.rs` | XL |
| [ ] | 事件订阅细化 | `aero-server/src/webhooks.rs` | L |

---

## 当前纪律

1. **禁止**在当前 Sprint 中修改以下模块（除非修复 bug）：
   - `aero-live-srt/`（SRT 协议已经稳定）
   - `aero-live-webrtc/`（SFU 媒体面）
   - `aero-live-whip/`（WHIP/WHEP）
   - `web/`（直到 REFACTOR 完成）

2. **每次修改后必须运行**：`make check-harness`（正常模式）或 `make check-rebase`（REFACTOR 模式）

3. **P0_REFACTOR 的 14 个 HARD 违规未清完之前**，禁止开始 Phase 2 的任何任务

4. **Phase 2 内**，按 P0 优先于 P1 优先于 P2 的顺序执行

## 决策流程（Agent 每日启动）

```
bash scripts/task-planner.sh
│
├─ HARD > 0  ──→ Phase 1: bash scripts/agent-start.sh
│                  → 选 Step → 拆分 → make check-rebase
│
├─ 依赖违规  ──→ 修复依赖方向
│
├─ 编译失败  ──→ cargo check 修复
│
└─ 全部通过  ──→ Phase 2: 读 CURRENT_SPRINT.md 看板
                   → 找第一个 [ ] 任务
                   → 实现 → make check-harness → [x]
                   → 更新 CURRENT_SPRINT.md
                   → 重复
```
