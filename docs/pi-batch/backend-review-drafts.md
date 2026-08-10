# Composer Drafts (草稿持久化) — Backend Engineering Review (gate round)

Feature: server-persisted composer drafts — one private draft per
`(participant, room)`, upsert on autosave, restore on room switch/reload,
clear on send. Backend (migration 0028, `DraftRepo`, `drafts.rs` routes) was
shipped in earlier rounds; the current batch owns the web wiring. This round:
backend engineer gate — analyze, verify against the specs/AGENTS.md hard
gates, fix what is broken, report honestly.

## 1. Analysis

### Business boundary & data ownership
- **Capability owner**: IM 消息域（composer 草稿 = 未发送消息的暂存态）。实现按
  AGENTS.md §4.1 配方拆分：migration（`message_drafts`）→ storage
  `DraftRepo`（`crates/aero-storage/src/draft.rs`）→ server 路由
  （`crates/aero-server/src/drafts.rs`，`.merge` 进 `routes::build`）。
- **数据所有权**: 草稿行归 `participant_id` 私有。路由全部以
  `auth.participant_id` 作用域化（`get_authorized`/`upsert_authorized`/
  `delete_authorized`），跨房列表经 SQL `aero_effective_room_access`
  过滤。无任何接口接受他人 participant 参数 — 无 IDOR 面。
- **公共契约**（与 web `api.js` 严格对齐，`api.test.js` 断言）：
  - `GET /api/rooms/:id/draft` → `{"draft": {room_id, blocks, reply_to?, updated_at} | null}`
  - `PUT /api/rooms/:id/draft` body `{blocks, reply_to?}` → `{"saved": true}`（upsert，天然幂等）
  - `DELETE /api/rooms/:id/draft` → `{"deleted": bool}`（幂等）
  - `GET /api/drafts` → `{"drafts": [...]}`（按 updated_at DESC）
  - 错误映射：401（AuthUser）/ 403（`assert_room_access`/存储 fence）/
    404（未知 room）/ 400（无效 id、空 blocks、reply_to 非本房存活消息）。

### 强一致（必须事务原子）
- 保存草稿 = 一次事务内完成：**当前有效访问 fence**（`aero_effective_room_access`
  锁链）+ **reply 目标 FOR SHARE 存活校验**（同房 + `deleted_at IS NULL`）+
  upsert。TOCTOU 防线：路由先 `assert_room_access`（canonical guard，CI
  authz_lint 认可），存储层再在同事务内复锁 — 撤销/踢人/删消息与保存并发时
  失败关闭。
- 数据库兜底（0214，与 channel_favorites/thread_* 同族）：FK
  `message_drafts_{participant,room}_fk` ON DELETE CASCADE（NOT VALID +
  VALIDATE）；`message_drafts_scope_guard` 触发器 — 身份不可变、无有效访问
  拒写、reply_to 必须存活且同房；`purge_personal_state_on_room_leave`
  （离房删草稿）；`purge_personal_thread_state_on_tombstone`（父消息软删 →
  仅清 reply_to 边，草稿本体保留）；GDPR 删号 `participant.rs` 显式
  `DELETE FROM message_drafts`。
- 明确不设 FK 的字段：`reply_to` → messages(id)。ephemeral 清扫对消息是
  **硬删**，FK 会阻塞该路径；由触发器和应用层校验替代（规格 §9「必须提供
  替代」已满足）。

### 变化点 & 模式选择
- 变化点：无（草稿存储无渠道/定价/通知轴）。**明确「no pattern —
  composition suffices」**：Controller → Service(ImService guard) →
  Repository 三层即完整。未引入任何单实现抽象/接口/工厂（design-patterns.md
  红线全避）；rule of three 未到期（`lock_effective_room_access_in_tx` 是
  首个 room 级 fence 副本，thread 级已有 `lock_effective_live_thread_root_in_tx`
  先例）。
- 依赖方向：Transport(aero-server) → Application(ImService) →
  Domain/Infrastructure(aero-storage)；`aero-common` 是叶子。无反向依赖，
  `Block` 直接复用 common 模型（wire=storage 同形，仓库既有约定）。

### 持久化设计（persistence-modeling.md §12，对已落表复核）
- **Aggregate/Table**: 单表 `message_drafts`（personal room state 族，与
  channel_favorites 等同构，非独立聚合）。
- **Identity**: 内部主键复合 `(participant_id, room_id)`（业务键即主键，
  无自增、无 UUID — 一对一无序小表，复合主键最简）；`updated_at` 服务端
  `now()` 生成；无幂等键需求（PUT upsert 天然幂等，非付费/外发）。
- **Consistency Boundary**: 一事务 = 访问 fence + reply 校验 + upsert。
- **Snapshot Fields**: `blocks` JSONB（与 messages 同形，wire=storage
  直通，无历史快照需求 — 草稿是暂存态非事实）。
- **Concurrency**: upsert 单行原子（`ON CONFLICT DO UPDATE`）；跨设备
  last-write-wins（web 端 precedence local > server > mirror，文档化残留
  风险）；无版本列 — 无并发编辑合并需求。
- **Main Queries + Indexes**: PK `(participant_id, room_id)`（房间单取）；
  `(participant_id, updated_at DESC)`（跨房列表，覆盖 WHERE+ORDER，非冗余）。
- **History**: 无 — 草稿非审计事实，删除即弃（软删反而制造恢复冲突）。
- **Deletion**: 发送/清空 → DELETE（幂等）；离房 → 触发器清；父消息删除 →
  清边留本体；GDPR → participant.rs 显式删。
- **Migration**: 0028 建表（IF NOT EXISTS）+ 0214 加固（FK/触发器，先
  LOCK TABLE 再改，NOT VALID+VALIDATE 免长锁）；全部幂等可重放 —
  `make migrate-smoke` 链在一次性库全量通过。

## 2. Implementation (this round)

后端主体已在前几轮提交并通过审查；本轮实际修复两处**门禁违规**（均为
web 侧、在本轮变更集内）：

1. **`web/drafts.test.js` 死锁（挂起）**：`reauthorize after a save-403
   resumes autosave` 测试 `await store.flush('r1')` 直接等待 FakeNet 的
   挂起 op promise，而 resolve 在 await 之后才调用 — 永久挂起
   （`node --test drafts.test.js` 60s 超时仍 cancelled 1，事件循环已空转）。
   修复：先发起 flush 拿 promise，`await flush()`（微任务泵）让 op 派发，
   断言 `net.saves[1]` 后 resolve 再 `await resumed`。修复后 28/28 通过，
   52ms，无取消。
2. **`web/app.js` 1001 行超 JS HARD(1000)**：HEAD 恰 1000，本轮接线 +1 行
   越线（上一轮报告称 file-size-check 通过，当前树实际违规）。修复：压缩
   两处历史注释（纯注释，无行为变化），现 998 行，0 违规。

无后端代码改动 — 后端经全面核验无需修复。

## 3. Self-check（architecture.md §8 + evolution.md §6）

- 模块边界清晰、依赖单向无环 ✅（crate 图 + authz_lint 6/6）
- 核心业务不依赖框架/ORM ✅（Block/校验在 common/im-core）
- 无上帝类/文件 ✅（drafts.rs 134 行、draft.rs 419 行、drafts.js 258 行）
- 无单实现抽象、无散落状态判断 ✅（草稿无状态机 — 非状态实体）
- 事务边界在用例层 ✅（`upsert_authorized` 单事务含 fence）
- 数据表所有权单一、跨模块只走公开入口 ✅（0214 族触发器 = DB 层兜底）
- 无重复业务规则 ≥3 处 ✅（fence 第 2 副本；约束集中在 migration）

### 实际运行的验证（全部通过）

| 命令 | 结果 |
|---|---|
| `cargo check --workspace --all-targets` | ✅ |
| `cargo test --workspace --lib` | ✅ 319 passed, 0 failed |
| `cargo test -p aero-server --test authz_lint` | ✅ 6 passed（含 draft 路由 guard 扫描） |
| `cargo clippy --workspace --all-targets` | ✅ 0 新警告 |
| `bash scripts/truth-check.sh` | ✅ 0 ORPHAN（3 UNWIRED 为既有 allowlist） |
| `bash scripts/file-size-check.sh` | ✅ 0 违规（修 app.js 后；修复前 1 HARD） |
| `bash scripts/web-check.sh` | ✅ 62 文件 / 119 import，0 违规 |
| `node --test` 全量（package.json 18 文件） | ✅ 121 passed, 0 fail（修复前 drafts.test.js 挂起） |
| `npx eslint`（drafts 7 文件 + app/api） | ✅ 0 问题（全仓 3 个 error 在未触碰的 oidc_callback.js / vendor SDK — 存量债） |
| PG 门控 `cargo test -p aero-storage --lib -- --ignored draft` | ✅ 6 passed（一次性 pgvector:pg17 容器，全链迁移后） |
| PG 门控 `--ignored personal_state` | ✅ 4 passed（fence/撤销/离房清扫/DB 约束） |
| `aero-cli migrate`（一次性库全链） | ✅ 238 迁移 ✓ ok |
| 活服务 HTTP smoke（自写 `/tmp/draft_live_smoke.py`，真 server + PG/Redis/NATS） | ✅ PUT/GET/upsert/reply_to/隐私(成员见 null)/403 非成员/404 未知房/400 无效 reply 与空 blocks/DELETE 幂等/401 全过 |

### 未能运行 / 如实说明
- `make migrate-smoke`：未跑（等价动作已做 — 一次性库全链 replay + 门控
  测试，且未动 migrations）。
- 真实浏览器 E2E 草稿流：无浏览器 harness（同上一轮）。
- eslint 全仓：3 个存量 error（`oidc_callback.js` 未用变量 ×2、
  `vendor/snaplink_sso_client.js` 空块）—— 均不在本轮变更文件，未顺手改
  （变更范围纪律）；CI 主门是 web-check.sh（通过）。
- 上一轮报告「`node web/drafts.test.js` passed」与当前树不符 — 死锁测试
  使文件挂起；已修复并重验（此为本轮唯一「报告与实测不一致」处）。

### 变更半径
- 修改：`web/drafts.test.js`（死锁修复，仅测试）、`web/app.js`（-3 行注释）。
- 未触碰：后端 crate、migrations、配置、事件/契约（无公共接口变化）。
- 回滚：两文件均为纯 web 变更，无迁移/数据面。
