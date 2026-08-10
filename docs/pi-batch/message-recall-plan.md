# 消息撤回（Message Recall）实现计划 v4 — 门禁第 3 轮 B1/B2 修复（failing-test-first）

- **日期**: 2026-08-06（v4；基于代码现场核查，锚点以当前 `git` 状态为准）
- **范围**: 发送方撤回自己的消息；内容替换为系统占位（`message_id`/房间历史/审计保留）；记录 `recalled_by` + `recalled_at`；广播 WS 事件全员渲染占位；仅作者或房间 owner/admin 可撤回，非作者 403；多租户边界不可穿越；验收含权限矩阵 + 迁移测试 + WS 事件形状
- **状态**: 计划（v4）。实现 + 5 项门禁修复 + 3 项测试缺口已在树且全绿；**门禁第 3 轮 REJECTED 于 2 项新发现（§0.5）：B1 撤回快照与附件 GC 的悬空引用（阻断，security/compliance 双确认）、B2 `npm test` 漏掉新测试文件（LOW）**——本计划给出 failing-test-first 修复。**本计划不含实现代码。**
- **前置**: `docs/specs/2026-05-22-aero-im-design.md`、`AGENTS.md`、backend-specs（architecture / persistence-modeling §12 / testing / agent-guardrails）、`docs/pi-batch/feature-gate.md`（第 3 轮 FAIL）、`docs/pi-batch/feature-reviews/*`、`/home/u1/ai-batch-runner/scripts/check-completion-report.py`

---

## 0. 需求评估与当前树状态

### 0.1 评估结果（`python /home/u1/ai-batch-runner/pi-batch.py assess`）

- 处方档 **demo（规模 S）**，工作流 **L3_platform**，产品化 **L2_platform_capability**；风险 low；硬规则 0 条。
- 信号：**多租户** → 克制落地：`tenant_id` 进主查询/唯一索引 → 复用 `messages.room_id → rooms.workspace_id` 既有边界（**不新建表**），`assert_room_access`（participant 在前）为唯一租户守卫；事件带组织上下文 → `RoomEvent::Recalled` 沿用 `im.room.{id}` subject（房间即租户边界）；审计字段 → 事务化 audit 行 + `message_edits` 快照；API 版本化 → `POST /api/messages/:id/recall` 挂既有 `/api/messages/:id` 资源族。
- 评估缺失项（main_flow / data_source / error_path / tech_stack）由本计划 §4 补全。

### 0.2 树内初版实现 + 门禁裁定

- 初版实现（59 文件未提交）在 `docs/pi-batch/feature-gate.md` 被 **REJECTED**，5 个可复现缺陷：
  1. **P1/HIGH — 系统编辑路径缺 `recalled_at` 围栏**（内容复活）：`edit_locked_outboxed_in_tx` 只围 `deleted_at`+`version`；`unfurl_bot` 慢网络 fetch 完成后 `edit_outboxed_system` 把原文写回占位 + 重建 FTS；DS-1 叠加：version 前进使 pending `Recalled` outbox 行被 relay 抑制启发式**静默丢弃**（整房无人看到占位）。
  2. **P1/P2/HIGH — change-replay 永不送达撤回**：`changes_since` 以 `GREATEST(edited_at, deleted_at)` 为键，撤回不 bump `edited_at` → 离线/重连客户端永久保留原文，`applyChange` 的 recall 分支是死代码。
  3. **P2 — 客户端 held-id 去重丢弃占位**：`app.js` 对已持有 id 直接 dedupe-skip，reconnect backfill 的占位行永不应用。
  4. **P2 — SPA 无撤回入口**：`ws.js`/`api.js`/`render.js` 无 `recallMessage`、无按钮（receive-only）。
  5. **HIGH — 迁移 0238 未重发 `backfill_messages_partition`**：影子表 backfill 投影缺 recall 列，分区 cutover 时静默丢撤回状态（徽标消失、占位可再编辑、回放断裂）。

### 0.3 修复已落树（逐项核查确认，§7 给回归测试）

| # | 缺陷 | 修复落点（已核查） | 回归测试（绿） |
|---|---|---|---|
| 1 | 系统编辑复活 | `events.rs`：`edit_locked_outboxed_in_tx`（行锁读后 `existing.recalled_at.is_some() → Ok(None)`）+ `update_voice_transcript_outboxed` 双围栏 | `system_edit_after_recall_is_fenced` |
| 2 | replay 不送达 | `query.rs:180-181` `GREATEST(edited_at, deleted_at, recalled_at)`（WHERE+ORDER BY）；0238 重发 `idx_messages_room_mutated` 三列表达式索引 | `changes_since_delivers_recalls` |
| 3 | 客户端去重丢占位 | `app.js:219` held-id 行若带 `deleted_at/recalled_at/edited_at` → 走 `applyChange` 守卫漏斗，仅真重投去重 | （web 测试钉漏斗） |
| 4 | SPA 无入口 | `ws.js:539 recallMessage`、`api.js:376 recallMessage`、`render.js` ↶ 按钮（self）、`app.js` 处理器带 **409→success 映射** | ws/api/render 测试 |
| 5 | backfill 缺列 | 0238 `CREATE OR REPLACE FUNCTION backfill_messages_partition` INSERT/SELECT 双投影含 `recalled_at, recalled_by` | `partition_backfill_carries_recall_columns` |

**计划 v2 的 3 个测试缺口已全部关闭（本轮核查在树）**：① `materialize_outbox_payload` Recalled 臂单测 `recalled_payload_is_delivered_at_version_and_suppressed_when_superseded`（delivered/superseded/tombstoned/gone 四分支）；② 并发双撤回竞态 `concurrent_double_recall_has_exactly_one_winner`（`tokio::join!` 恰一 `Ok(Some)` 一稳定 409、恰一 outbox 行）；③ web recall 测试（`ws.test.js` 帧形状 + `_lastSeen` 卫生、`api.test.js` 端点 + 409 ApiError、新 `render_recall.test.js` 按钮可见性矩阵 + 占位渲染）。

**额外客户端缺陷（由新测试先红后修）**：`web/ws.js` `_lastSeen` 原先对任何含 `message.id` 的已应用帧（含 recalled/edited/deleted）推进——一条「客户端从未收到 create」的变更帧会让 legacy `?since=` backfill 跳过该消息整场会话。已改为仅 `msg.type === 'message'` 帧推进（与 `_queueDeliveryAck` 策略一致；变更类经 `changes_since` 收敛）。

### 0.4 上一轮 implement 阶段 VALIDATION_FAILED 根因（本次必须修复）

- 流水线（`/home/u1/aero-im-batch/backend-feature-pipeline.yaml`）对 implement 产物跑 5 个验证器：`cargo-check` / `cargo-clippy` / `cargo-test`（repo 域，全过）、`backendquality`（过，0 违规）、**`completion` = `python /home/u1/ai-batch-runner/scripts/check-completion-report.py {output}`（file 域，**exit 1**）**。
- 上一轮回复以**散文式**完成报告收尾；检查器机械要求 **```yaml completion_report: ...``` 围栏块**，临时产物被拒、未提交（`docs/pi-batch/feature-implementation.md` 仍是首轮旧报告）。
- 检查器规则（任一命中即 exit 1）：缺 `completion_report` 块；`commands_executed` 为空或缺 `result ∈ {passed, failed, not_executed}`；`not_executed` 条目缺 `reason`；`changed_files` 为空；伪造通过措辞（`理论上应该通过`/`should pass`/`assume passed` 等）。
- **修复契约见 §8.5**：实现者最终回复必须以内嵌 YAML completion_report 块收尾，且产物写入流水线输出路径 `docs/pi-batch/feature-implementation.md`；交付前本地跑 `python /home/u1/ai-batch-runner/scripts/check-completion-report.py docs/pi-batch/feature-implementation.md` 必须 `COMPLETION: OK`。

### 0.5 门禁第 3 轮裁定（GATE_REJECTED，2 项新发现——本次修复范围）

门禁在「5 缺陷全修、3 缺口全关、全门绿」的基础上再拒，阻断项为**撤回事务自身引入的附件悬空引用**：

| # | Sev | 发现（已源码核实） | 修复（failing-test-first） |
|---|---|---|---|
| **B1** | MED/HIGH（阻断） | 撤回事务（`authorization.rs:284-333`）把含 `File`/`Voice` `blob_id` 的原始 blocks 快照入 `message_edits` **随后**对同一批 `blob_id` 调 `enqueue_unreferenced_blobs_in_tx`；入队检查（`crud.rs:306-340`）与 drain 时 `has_live_references`（`blob.rs:328-354`）只扫 `messages`（+emoji/ledger），**不扫 `message_edits`**；`GET /api/messages/:id/history`（`message_history.rs:64`）对 live-recalled 行成员可见。**该缺陷是撤回独有**：删除路径不写快照、编辑路径从不 GC 被替换块。效果：附件字节 ~60s 内被销毁，历史证据仍指向死字节。security/compliance 双确认。 | **快照脱敏（Option A）**：`message_edits` 快照中 `File`/`Voice` 块不保留 `blob_id`（File → `[附件已移除]` 文本块；Voice → 保留 transcript 为文本块 / 无 transcript 则 `[语音已移除]`）；GC 仍按原始 blocks 入队。详见 §2 Snapshot Fields 决策记录。 |
| **B2** | LOW（特性范围） | `web/package.json` `"test"` 脚本显式列 14 个文件，**漏掉新增的 `render_recall.test.js`**；CI（`.github/workflows/ci.yml:168`）跑 `npm test` → 新渲染测试永不进 CI。harness 门（glob）覆盖但仓库规范门不覆盖。 | `web/package.json` test 脚本追加 `render_recall.test.js`（`render_giphy.test.js` 之后）；验证 `npm test`（web/ 目录）全绿。 |

**门禁已驳回项（实现者不要动）**：`/changes` 200 行单页 clamp（既有端点限制，≤200 变更/重连窗口内收敛有保证）、迁移 `statement_timeout=10s`（规模条件性、既有池配置、无生产部署路径）、reactions 在 mutation replace 后消失（编辑路径既有）、REST 响应丢弃/WS-down 误导提示（收敛有保证，UX polish）、无提交锁/幂等键（409 兜底良性）、401 未走 forceReauth、客户端漏斗无自动化测试（测试缺口，服务端回归已被变异实验证明可捕获）、deferred-erasure 谓词漏 recalled 行（产品/法务决策）、devops C1/H1-H4/M 项（仓库级既有交付层问题，非特性引入）。

---

## 1. 模块边界与数据所有权（architecture.md）

### 1.1 决策摘要

| 决策 | 选择 | 理由 |
|---|---|---|
| 撤回语义 | **软状态替换**：`recalled_at`/`recalled_by` 置位 + `blocks` 换系统占位 + `searchable_text=''` + `embedding=NULL`，行保留（`deleted_at` 不置位） | 需求「keeps message_id/room history/audit」；与 `deleted_at` 墓碑正交，二者可共存（先撤回后删除） |
| 权限 | 作者 **或** 房 `owner`/`admin`（`migrations/0001_init.sql:54` CHECK `role IN ('owner','member','admin')`，「moderator」即 admin 语义）；房 member 非作者 → 403；非成员/跨租户 → 403；未知消息 → 404 | 需求原文 |
| 事件 | `RoomEvent::Recalled(Message)`（tuple 变体，镜像 `Edited(Message)`）+ WS 帧 `{"type":"recalled","message":{...}}` | 与 `Edited`/`Deleted` 同 fan-out 路径；tuple 变体无 `kind` 字段撞名风险（AGENTS §4.2）；客户端直接渲染占位 |
| 历史/审计 | 原内容快照入 `message_edits`（`editor_id`=撤回者）+ `audit_log` 追加 `message.recalled`，同事务 | 复用既有证据表，不建新表；「撤回后原正文仍可审计」 |
| 附件 | 原 `File`/`Voice` 块 blob 入 `blob_gc_queue`（事务内引用检查后入队） | 撤回即内容移除；字节不残留 |
| 副作用 | 不产 Embed/Moderate 作业；`embedding=NULL`；bus AI answer-cache 失效收口扩 `Edited\|Deleted\|Recalled` | 撤回是隐藏而非新内容 |

### 1.2 数据所有权（谁写什么）

- **`aero-storage` `MessageRepo`**：`messages` 行（含 recall 列）唯一写入者。`recall_outboxed_authorized`（授权 + 占位 + 审计 + outbox 单事务，`authorization.rs`）、`recall_role_allowed_in_tx`（事务内角色复核）、`edit_locked_outboxed_in_tx` / `update_voice_transcript_outboxed` 的 recall 围栏（`events.rs`）、`changes_since` 三列谓词（`query.rs`）、`EventOutboxKind::Recalled`（`event_outbox.rs`）。`audit_log`/`message_edits`/`event_outbox` 经各自既有 `*_in_tx` 同事务写入。
- **`aero-im-core` `ImService`**：用例编排 `recall_message`（`service/messages.rs:460`）+ 纯权限函数 `recall_authorized`（:22，可单测）+ `editable_message` 拒撤回 + `materialize_outbox_payload` Recalled 臂（`service/outbox.rs:263`）。`assert_room_access` 仍是唯一租户守卫。
- **`aero-server`**：REST handler（薄壳）+ WS `ClientFrame::RecallMessage`（`ws_impl/mod.rs:150`）+ `ServerFrame::Recalled`（:348）+ `room_event_to_frame_json` 臂（`ws/frame.rs:30`）+ bus AI-cache 失效（`bus.rs`）。
- **`aero-common`**：`Message` 新字段、`RoomEvent::Recalled`、`RECALLED_MESSAGE_PLACEHOLDER` 常量、`MESSAGES_RECALLED_TOTAL` 指标。
- **`web/`**：`ws.js`/`api.js` 入口 + `app.js` 漏斗/处理 + `render.js` 渲染，只读消费，不持有状态权威。

依赖方向不变：`server → im-core → storage → common`；不新增 crate、不新增第三方依赖。

---

## 2. Persistence Design 报告（persistence-modeling.md §12，编码前强制输出）

### Aggregate
`Message`（`messages` 行）是唯一被操作的聚合。状态机**集中定义**（代码层，不加跨列 CHECK——`recalled_at` 与 `deleted_at` 可共存）：

```
live (deleted_at IS NULL, recalled_at IS NULL)
  ├─ edited   (version+1)                    —— 仅 live
  ├─ recalled (version+1, recalled_at/by, blocks=占位)  ← 本次新增；编辑从此封死
  │    └─ deleted (deleted_at, blocks=[])    —— 撤回后可再删除（终态墓碑）
  └─ deleted  (deleted_at, blocks=[])        —— 终态，不可撤回/不可编辑
```

### Tables
**唯一 schema 变更：迁移 `0238_message_recall.sql`**（已落树，全链可重放）：

```sql
ALTER TABLE messages ADD COLUMN IF NOT EXISTS recalled_at TIMESTAMPTZ,
                     ADD COLUMN IF NOT EXISTS recalled_by UUID REFERENCES participants(id);
-- messages_partitioned（0148 LIKE 快照影子表，0174 起约定 live 新列必须显式镜像）
ALTER TABLE messages_partitioned ADD COLUMN IF NOT EXISTS recalled_at TIMESTAMPTZ,
                                 ADD COLUMN IF NOT EXISTS recalled_by UUID;
UPDATE messages_partitioned AS shadow SET recalled_at = live.recalled_at, recalled_by = live.recalled_by
  FROM messages AS live
 WHERE shadow.id = live.id AND shadow.created_at = live.created_at
   AND shadow.recalled_at IS DISTINCT FROM live.recalled_at;   -- 幂等 reconcile
-- event_outbox.kind CHECK 扩展（0211 的 drop+add 模式，7 kind 全保留）
ALTER TABLE event_outbox DROP CONSTRAINT IF EXISTS event_outbox_kind_check;
ALTER TABLE event_outbox ADD CONSTRAINT event_outbox_kind_check CHECK (
    event_kind IN ('message','edited','deleted','notify','reaction','canvas_op','recalled'));
-- change-replay 表达式索引重发（0125 原二列 → 三列；plain index，DROP+CREATE 安全）
DROP INDEX IF EXISTS idx_messages_room_mutated;
CREATE INDEX IF NOT EXISTS idx_messages_room_mutated
    ON messages (room_id, GREATEST(edited_at, deleted_at, recalled_at));
-- 影子表 backfill 函数重发（0148/0149/0158/0174 约定）：INSERT/SELECT 双投影含 recall 列
CREATE OR REPLACE FUNCTION backfill_messages_partition(batch_size INT DEFAULT 5000,
    from_id UUID DEFAULT '00000000-0000-0000-0000-000000000000')
RETURNS TABLE (rows_copied BIGINT, last_id UUID) ...;
```

不加跨列 CHECK（状态机由代码保证）；不加 `reason` 列（YAGNI）。

### Identity
| 维度 | 设计 |
|---|---|
| 内部主键 | `messages.id`（既有 `MessageId` ULID，不动） |
| 业务键 | `(room_id, sender_id)` 不可变（`lock_message_in_tx` FOR UPDATE + 事务内身份复核，防并发改房/改作者越权） |
| 幂等键 | **无新键**。撤回是单行原子状态迁移；outbox 事件以 `event_id` + per-subject seq 去重（§4.3） |

### Consistency Boundary（一个事务保存什么）
`recall_outboxed_authorized` 单事务（顺序固定）：
1. `resolve_message_target`（无锁预读 room/sender，None → NotFound）
2. `lock_effective_message_write_access(PostPolicy::Ignore)`（`aero_effective_room_access` + rooms `FOR SHARE` 租户/成员围栏）→ None → Forbidden
3. `lock_message_in_tx`（`FOR UPDATE`）→ 复核 room/sender；`deleted_at`/`recalled_at` 状态复核
4. `recall_role_allowed_in_tx`：事务内 `SELECT role FROM room_members ... FOR UPDATE` 复核 admin/owner（防并发改角色 TOCTOU）
5. `INSERT message_edits`（原 blocks 快照，`editor_id`=撤回者）
6. `UPDATE messages SET blocks=占位, searchable_text='', embedding=NULL, recalled_at=NOW(), recalled_by=$actor, version=version+1 WHERE id=$1 AND recalled_at IS NULL AND deleted_at IS NULL`（原子双保险；0 行 → 409 raced）
7. `enqueue_unreferenced_blobs_in_tx`（原附件 GC，先查 live 引用）
8. `AuditRepo::append_in_tx`（`message.recalled`，workspace = rooms.workspace_id，actor=撤回者，detail 含 room_id + 120 字 digest）
9. `EventOutboxRepo::insert_room_event_in_tx`（kind=`Recalled`，payload=`RoomEvent::Recalled(占位消息)`，`aggregate_version` = 新 version）

任一失败整体回滚：**消息永不「被撤回却无审计/无事件/无快照」**。事件发布走既有 outbox relay（`dispatch_event_outbox_id` 快路径 + 250ms batch 兜底），跨实例扇出复用 `run_bus_listener`，零新总线代码。

### Snapshot Fields
- `message_edits` 快照**脱敏后的** `blocks`（v4 门禁 B1 修复）：`File`/`Voice` 块的字节引用（`blob_id`）**不写入快照**——`Block::File` → `Block::Text("[附件已移除]")`；`Block::Voice{transcript: Some(t)}` → `Block::Text(t)`（保留文字证据，无字节引用）；`Block::Voice{transcript: None}` → `Block::Text("[语音已移除]")`；其余块原样保留。**GC 仍按原始 blocks 的 `blob_id` 入队**（字节按计划意图移除），但快照不再指向死字节。
- `audit_log.detail` = `{room_id, digest}`（digest = 原 `searchable_text` 前 120 字符，与删除路径同款，无字节引用）。
- 不存发送方快照（`sender_id` 在行上不可变）。

### 为什么选脱敏而非扩展引用扫描（B1 决策记录）
- `message_edits` 在墓碑清理的 **retained** 列表（`crud.rs:539` 断言）——删除消息的历史证据**永久保留**。若把 `message_edits` 纳入 `has_live_references`/入队检查（Option B），被删除消息历史引用的字节将永久受保护 → 删除路径 GC 失效、字节泄漏；且每次消息写事务与 GC drain 都要扫描 append-only 证据表（热路径成本）。
- Option A（脱敏快照）零热路径改动、按构造无悬空引用、与「撤回 = 内容移除、证据 = 文本快照 + 审计 digest」的产品语义一致。附件字节仍按计划回收（不泄漏）。
- 已知取舍：历史中的附件元数据（`blob_id`/`size`）不保留——这正是「撤回移除内容」的意图；编辑路径快照仍含 `blob_id` 但编辑路径从不 GC 被替换块（既有泄漏，不在本特性范围，已记录）。

### Concurrency
- 行级 `FOR UPDATE` + UPDATE `WHERE recalled_at IS NULL AND deleted_at IS NULL` 双保险：并发双撤回恰一赢家（后到者行锁复核 → 409 raced）。
- `version = version + 1` 与编辑共用乐观锁；outbox `UNIQUE (message_id, aggregate_version)` 保证同版本只产一个事件。
- 角色在事务内 `FOR UPDATE` 复核（TOCTOU）。
- 并发「撤回 vs 删除」：行锁串行化，后到者看到已变状态 → 稳定 409/幂等。
- **系统编辑围栏（门禁缺陷 1）**：`edit_locked_outboxed_in_tx` / `update_voice_transcript_outboxed` 在行锁读后对 `recalled_at.is_some() → Ok(None)`——**围栏在写行的唯一事务里，不依赖调用方**（unfurl/transcribe/webhook 全部被围）。

### Main Queries + Indexes
| 查询 | 载体 | 索引 |
|---|---|---|
| 定位消息 + 行锁 | `SELECT ... FROM messages WHERE id=$1 FOR UPDATE` | PK |
| **change-replay（门禁缺陷 2）** | `WHERE room_id=$1 AND GREATEST(edited_at, deleted_at, recalled_at) > $2 ORDER BY GREATEST(...) ASC` | `idx_messages_room_mutated`（0238 三列表达式重发） |
| 历史/单条读取（占位可见） | 既有 `get`/`query.rs` 全 `MessageRow` SELECT | PK/既有索引；占位不进 FTS（`searchable_text=''`） |
| 角色复核 | `SELECT role FROM room_members WHERE room_id=$1 AND participant_id=$2 FOR UPDATE` | 既有 `(room_id, participant_id)` PK |

无新表、无新查询路径；撤回按 PK 定位。

### History
- `message_edits` 追加一行（原文快照，时间序）；既有 `GET /api/messages/:id/edits` 历史路由直接可见撤回前正文（「撤回不是丢失，是可审计」）。
- `audit_log` 追加 `message.recalled`（与 `message.deleted`/`message.moderated` 平级）。

### Deletion
- 撤回**不删行、不清 visible associations**（reactions/pins/bookmarks/notifications/read_receipts 保留——撤回 ≠ 墓碑；UI 抑制交互）。
- 撤回后仍可走既有删除路径（墓碑 + 关联清理），正交。
- 附件字节走既有 `blob_gc_queue` 异步回收（仅当无其他 live 引用）；**快照已脱敏**（v4 B1），GC 后历史视图不会出现悬空附件。

### Migration（Expand–Migrate–Contract）
- 纯 additive：2 可空列 + CHECK 扩展 + 影子镜像 + 索引重发 + backfill 投影重发；无回填（NULL=未撤回）、无双写、无读取切换。
- 顺序：`cargo build`（迁移编译期嵌入 `aero-storage/db.rs`）→ `aero-cli migrate` → 发新代码。旧代码读新列无感（serde default/skip）。
- 回滚（先回滚二进制）：`DELETE FROM event_outbox WHERE event_kind='recalled' AND published_at IS NULL`（防旧 relay 解码失败卡聚合）→ DROP 两列 ×2 表 → CHECK 回退不含 `recalled`。
- 大表/锁风险：`ADD COLUMN` 元数据操作无锁；镜像 reconcile 按 `(id, created_at)` 匹配、NULL-safe、可重放；`idx_messages_room_mutated` DROP+CREATE 短暂丢索引（plain index，非约束）。
- **部署纪律（DS-4）**：recall 上线前**全节点先升级**——旧二进制对未知 `kind:"recalled"` 走 poison ack-drop 且 durable cursor 前进（永久丢失）；写入 release notes。

---

## 3. 状态机与权限（集中定义）

- 状态机：见 §2 Aggregate（`live → recalled → deleted`，编辑仅 live；撤回后编辑 409）。**判断只在 storage 事务与 `messages.rs` 集中，不散落 controller/consumer**。
- 纯函数 `recall_authorized(actor, sender_id, room_role) -> Result<(), Error>`：author → Ok；`Owner|Admin` → Ok；member 非作者 → `Forbidden("only author or room admin may recall")`。

---

## 4. API 契约

### 4.1 主流程（main_flow，评估缺失项补全）

```
Client ── POST /api/messages/:id/recall ──▶ handler（薄壳）
        ◀── 200 {message(占位)} ──┘
        │  ImService::recall_message(actor, id)：
        │    1. messages.get(id) → None → 404 NotFound
        │    2. assert_room_access(actor, room)          ← 租户/成员/停用/2FA 唯一守卫（participant 在前）
        │    3. deleted → 409 "message is deleted"
        │    4. recalled → 409 "message is already recalled"
        │    5. recall_authorized(actor, sender, role) → 403 "only author or room admin may recall"
        │    6. messages.recall_outboxed_authorized(...) ← 事务（§2 Consistency Boundary）
        │    7. dispatch_event_outbox_id（快路径；失败 batch 兜底）
        └─▶ bus im.room.{id} → run_bus_listener（durable, 成员展开）→ hub.fan_out_raw → 全员
             WS 帧 {"type":"recalled","message":{...,"recalled_at":...,"recalled_by":...,
                     "blocks":[{占位}],"version":N},"seq":N}
             web: ws.on('msg:recalled') → handleRecalled → 原地替换 + 渲染占位（已撤回徽标，抑制操作）
```

数据来源（data_source）：请求仅 `:id`；消息/房间/角色/审计全部来自 PG；事件经 NATS `im.room.*` durable consumer 扇出（与 Edited/Deleted 同路）。

### 4.2 REST 端点

| 端点 | 语义 | 成功 | 失败 |
|---|---|---|---|
| `POST /api/messages/:id/recall` | 撤回（作者/admin/owner） | `200` + 完整 `Message` JSON（占位 blocks + `recalled_at`/`recalled_by` + `version` 递增） | §4.3 错误表 |
| `PATCH /api/messages/:id`（既有） | **撤回后拒绝编辑**：`editable_message` 增 `recalled_at.is_some()` → 409 | — | — |
| `DELETE /api/messages/:id`（既有） | 撤回后可再删除，不变 | — | — |
| `GET /api/messages/:id`、`GET .../history`（既有） | 撤回消息仍可读（占位），`deleted_at` 过滤不变 | — | — |

### 4.3 稳定错误模型（code 来自 `aero_common::Error::code()`，msg 为稳定常量，测试可断言）

| 场景 | HTTP | `code` | 稳定 `msg` |
|---|---|---|---|
| 未知消息 | 404 | `not_found` | `"message {id} not found"`（沿用现有格式） |
| 非成员/跨租户（房间不可见） | 403 | `forbidden` | `assert_room_access` 既有文案 |
| 房 member 非作者 | 403 | `forbidden` | `"only author or room admin may recall"` |
| 已删除 | 409 | `conflict` | `"message is deleted"`（与编辑同款） |
| 已撤回 | 409 | `conflict` | `"message is already recalled"` |
| 并发状态翻转 | 409 | `conflict` | `"message recall raced with another mutation"` |

响应体统一 `{"code","msg"}`（`ApiError::into_response` 既有形状）。**判定顺序固定**：`get → 404`；`assert_room_access → 403`；`deleted → 409`；`recalled → 409`；`role → 403`——跨租户调用者永远拿不到状态信息（防存在性 oracle）。

### 4.4 权限矩阵（验收核心，表驱动单测）

| actor \ 状态 | 作者 | 房 admin | 房 owner | 房 member（非作者） | 房外成员/跨工作区 | 非成员 |
|---|---|---|---|---|---|---|
| live 消息 | ✅ | ✅ | ✅ | ❌ 403 | ❌ 403 | ❌ 403 |
| 已撤回消息 | ❌ 409 | ❌ 409 | ❌ 409 | ❌ 403* | ❌ 403 | ❌ 403 |
| 已删除消息 | ❌ 409 | ❌ 409 | ❌ 409 | ❌ 403* | ❌ 403 | ❌ 403 |
| 未知消息 | ❌ 404（不泄露存在性） | | | | | |

\* 状态检查严格在租户守卫之后（§4.3 顺序）。

### 4.5 WS 契约

- 客户端→服务端：`ClientFrame::RecallMessage { id }`（`ws_impl/mod.rs:150`，与 `DeleteMessage` 同构；分发臂 `ws_impl/frame.rs:157`）。
- 服务端→客户端：`ServerFrame::Recalled { message }`（:348），`room_event_to_frame_json`（`ws/frame.rs:30`）→ `{"type":"recalled","message":{...},"seq":N}`；`seq` 照常注入（at-least-once 重投客户端按序去重）。
- `RoomEvent::Recalled(Message)`：tuple 变体（无 `kind` 撞名）；`explicit_recipients()` 空（全员扇出）、`room_id()` = `message.room_id`。
- `run_bus_listener` AI answer-cache 失效收口扩为 `Edited | Deleted | Recalled`。

### 4.6 占位内容（确定性产物）

- `aero_common::RECALLED_MESSAGE_PLACEHOLDER = "[此消息已被撤回]"`；落库 `blocks = [Block::text(占位)]`，`searchable_text=''`（不进 FTS/向量），`embedding=NULL`。
- 客户端：`render.js` `isRecalled = Boolean(m.recalled_at)` → 灰字「· 已撤回」徽标 + 渲染占位 blocks；抑制 react/reply/edit/recall 操作行。

### 4.7 幂等策略

- **无外部副作用** → 不需要幂等键表。撤回 = 单行原子状态迁移（`FOR UPDATE` + `WHERE recalled_at IS NULL AND deleted_at IS NULL`），重复/并发请求**至多一次**生效。
- 重复撤回返回**稳定 409**（需求明确 already-recalled 是失败路径）；客户端对自身重试将 409 映射为成功（`app.js:666-669`，文档化约定——撤回是终态且内容等价，安全）。
- 事件侧：outbox `event_id` 作 NATS Msg-Id 去重 + per-subject seq 单调持久（重投同 seq）；客户端 `handleRecalled` 镜像 `handleEdited` 乱序守卫（`lastEditAt` 顺序键；recall 不 bump `edited_at`、撤回后编辑封死 → 该键在服务端不变量下健全）。

---

## 5. 测试计划（testing.md 五层；门禁缺陷修复一律 **failing-test-first**）

### 5.1 单元（领域规则/纯函数，不连 DB）

| 用例 | 位置 | 断言 |
|---|---|---|
| 权限矩阵全表驱动（作者/admin/owner/member/非成员/跨房 × live/recalled/deleted/unknown） | `aero-im-core` `messages.rs` 测试段 | 允许/403/409/404 全组合（已落树） |
| `RoomEvent::Recalled` serde round-trip（`kind:"recalled"` tag、字段齐全、`explicit_recipients` 空、`room_id` 正确） | `aero-common` model tests | 已落树 |
| `Message` 新字段 serde（缺省反序列化兼容旧 JSON） | `aero-common` model tests | 已落树 |
| 占位常量形状 | `aero-common` | 已落树 |
| `materialize_outbox_payload` Recalled 臂（四分支） | `aero-im-core/src/service/outbox.rs` 单测 `recalled_payload_is_delivered_at_version_and_suppressed_when_superseded` | kind=Recalled、version V、live 行 version V+1 → `None`（suppressed）；live version == V → `Recalled` 带当前占位（delivered）；行已删/行消失 → `None`（已落树，绿） |
| **NEW（B1 修复，先写先红）：快照脱敏纯函数** `redact_blocks_for_recall_snapshot` | `aero-storage/src/message/mod.rs`（挨着 `attached_blob_ids`，hermetic） | `File` → `[附件已移除]` 文本块且无 `blob_id`；`Voice{transcript: Some}` → 文本块含 transcript；`Voice{transcript: None}` → `[语音已移除]`；Text/Mention/Card 等其余块原样；**输出块集合不含任何 `blob_id`** |

### 5.2 契约（WS 事件形状，验收点）

| 用例 | 位置 | 断言 |
|---|---|---|
| `recalled_frame_shape_carries_placeholder_message` | `ws/frame.rs:225` | `type=="recalled"`、`seq` 注入、`message.blocks[0].content==占位`、`recalled_at/recalled_by` 存在、与 `edited` 帧互不混淆（已落树） |
| 可选（非阻塞，覆盖债）：帧序测试（edit→recall / recall→stale-edit 两序列） | `ws/frame.rs` 或 web ws.test | recall 后到达的 edited 帧被客户端顺序守卫丢弃（钉服务端不变量，防未来回归） |

### 5.3 集成（DB，`#[ignore]`+`DATABASE_URL`，一次性库）

`aero-storage/src/message/recall_tests.rs`（已落树 8 用例）：

| 用例 | 覆盖 |
|---|---|
| `recall_schema_columns_and_outbox_kind_are_applied` | 迁移验收：`information_schema` 两表 recall 列 + CHECK 含 `recalled`；旧行 NULL |
| `author_recall_replaces_content_and_records_audit_in_one_tx` | 占位写入、`recalled_by/at`、`version+1`、`message_edits` 快照（editor=撤回者）、audit `message.recalled`、outbox 行（kind=`recalled`、aggregate_version=新 version） |
| `recall_permission_matrix_author_admin_owner_member` | 提交时围栏矩阵：author/admin/owner Ok；member Forbidden |
| `recall_cannot_cross_workspace_boundaries` | 跨工作区 Forbidden 且不泄露状态 |
| `recalled_message_can_still_be_deleted` | 撤回后删除 = 墓碑成功（正交） |
| `system_edit_after_recall_is_fenced`（**门禁缺陷 1 回归**） | recall → `get_version` → `edit_outboxed_system(原文, cur_version)` → `Ok(None)`，blocks==占位、version 不变、FTS 未重建。**先写先红（已修复，测试在树）** |
| `changes_since_delivers_recalls`（**门禁缺陷 2 回归**） | 建消息 → 游标 `since=now-1min` → recall → `changes_since(room, since, 50)` 返回该行且 `recalled()`。**先写先红（已修复，测试在树）** |
| `partition_backfill_carries_recall_columns`（**门禁缺陷 5 回归**） | 执行 backfill（`from_id` 游标）→ 影子行 `recalled_at/by` 与 live 一致。**先写先红（已修复，测试在树）** |
| **并发双撤回竞态（缺口 ②，已关闭）** | `concurrent_double_recall_has_exactly_one_winner` | `tokio::join!` 两路 `recall_outboxed_authorized` → 恰一 `Ok(Some)` 一 `Conflict("message is already recalled")`；blocks==占位；`version==orig+1`；**恰一条 outbox 行**（已落树，绿；9/9 storage recall 全绿） |
| **NEW（B1 回归，先写先红）：撤回快照不含字节引用且 GC 照常入队** | `recall_tests.rs` | 消息含 `Block::File{blob_id}` + `Block::Voice{blob_id, transcript: Some}` → 撤回 → ① `message_edits` 快照行 `blocks` 的 JSON **不含任一 blob_id** 且含 transcript 文本；② `blob_gc_queue` 有该两 blob 的行（字节仍按计划回收，不泄漏）；③ 原消息行 `blocks` == 占位。先写先红（现状：快照含 blob_id） |

`aero-im-core/src/db_tests/recall_tests.rs`（已落树 3 用例）：`recall_success_returns_placeholder_and_author_flow`、`recall_admin_can_recall_but_member_and_stranger_cannot`、`recall_cross_room_member_is_forbidden_without_state_leak`——404/403/409 顺序、admin 提权、跨房无 oracle。

### 5.4 E2E（冒烟脚本，前台起服务 + 一次性库 + `pkill aero-server`）

注册 2 人 → 建房 → A 发消息 → A 撤回 → 房内双端 WS 收 `msg:recalled` 渲染占位 → B 撤回 → 403 → 重复撤回 → 409 → 刷新历史仍见占位行 → 离线重连（`changes_since` 游标）收到撤回。**浏览器真实 E2E 无 harness，如实标注不执行**（以 5.2 契约测试 + render.js 静态路径 + web-check 兜底）。

### 5.5 非功能

- 并发竞态（5.3 已关闭）；`scripts/{truth-check,file-size-check,web-check}.sh` 0 违规（app.js 已在 1000 行 HARD 线内）；`cargo clippy --workspace --all-targets` 0 新增警告；`cargo check --workspace` 干净。
- **web 测试（缺口 ③，已关闭）**——`web/*.test.js` 现含 recall 覆盖（全库 node --test 82 用例绿）：
  - `ws.test.js`：`recallMessage(id)` 发送 `{"type":"recall_message","id"}`；`mutation frames never advance the legacy backfill cursor`（`_lastSeen` 仅 `type==='message'` 帧推进——本次新修）。
  - `api.test.js`：`recallMessage(id)` 调 `POST /api/messages/:id/recall`（path-encoded、无 body）；**409 → `ApiError(status=409, code=conflict)`** 供调用方映射为成功。
  - `render_recall.test.js`（新文件）：按钮可见性矩阵（作者可见 / 非作者不可见 / recalled 行无任何操作按钮）；`isRecalled` 渲染徽标 + 占位 + 抑制操作行（已落树，绿）。
- **NEW（B2 修复，先写先红）：仓库规范测试门**——`web/package.json` `"test"` 脚本显式文件列表**漏掉 `render_recall.test.js`**（CI `.github/workflows/ci.yml:168` 跑 `npm test` → 新渲染测试永不进 CI）。修复：列表追加 `render_recall.test.js`；验证 `cd web && npm test` 全绿（含该文件）且 `node --test web/*.test.js` 仍 82+ 用例绿。

### 5.6 禁止项（testing.md §6）

禁止 skip 失败测试、改预期凑绿、无断言用例、`sleep` 硬等；PG 门控全部 `#[ignore]` + `-- --ignored`，测试自建一次性 participant/workspace/room（沿用 db_tests 惯例；不清库为既有约定，不背新债）。

---

## 6. 变更半径（agent-guardrails.md）

### 6.1 直接修改文件（初版已落树；修复轮新增标 ★）

| 文件 | 变更 |
|---|---|
| `migrations/0238_message_recall.sql` | **新增**：两表加列 + 影子 reconcile + outbox CHECK 扩展 + `idx_messages_room_mutated` 三列重发 + `backfill_messages_partition` 投影重发 ★ |
| `crates/aero-common/src/model/{message,event}.rs` | `Message` + `recalled_at/recalled_by`（serde default/skip）；`RoomEvent::Recalled(Message)` + match 臂；占位常量 |
| `crates/aero-common/src/metrics.rs` | `MESSAGES_RECALLED_TOTAL` + histogram op `"recall"` |
| `crates/aero-storage/src/message/{mod,query,crud,events,authorization,thread,sweep,search,orig}.rs` | `MessageRow` 两列；**全部 `SELECT ... FROM messages`（约 15 处）补列**（sqlx `query_as` 缺列即运行时解码错）；`editable_message` 侧拒撤回；`recall_outboxed_authorized`/`recall_locked_outboxed_in_tx`/`recall_role_allowed_in_tx`；`changes_since` 三列谓词 ★；`events.rs` 系统编辑双围栏 ★；**`mod.rs` 新纯函数 `redact_blocks_for_recall_snapshot`（B1）+ `authorization.rs` 撤回事务快照改走脱敏（B1）** ★ |
| `crates/aero-storage/src/message/recall_tests.rs` | **新增**：8 用例 + 并发竞态（缺口 ② 已关闭）+ **B1 快照脱敏回归（先写先红）** ★ |
| `crates/aero-storage/src/event_outbox.rs` | `EventOutboxKind::Recalled` + as_str + TryFrom |
| `crates/aero-im-core/src/service/{messages,outbox}.rs` | `recall_message` + 纯函数 `recall_authorized` + `editable_message` 拒撤回；`materialize_outbox_payload` Recalled 臂 + 缺口 ①单测（已关闭）★ |
| `crates/aero-im-core/src/db_tests/recall_tests.rs` | **新增**：3 用例 |
| `crates/aero-server/src/routes/handlers/messages.rs` + `routes/routes.rs` | `POST /api/messages/:id/recall` + 挂载 |
| `crates/aero-server/src/ws/ws_impl/{mod,frame}.rs`、`ws/frame.rs` | `ClientFrame::RecallMessage` + `ServerFrame::Recalled` + 分发臂 + `room_event_to_frame_json` 臂 + 契约测试 |
| `crates/aero-server/src/ws/ws_impl/bus.rs` | AI answer-cache 失效收口加 `Recalled` |
| `crates/aero-server/src/bot_dispatch.rs`、`webhooks.rs`、`forward.rs` | kind 映射/转发过滤（forward 只滤 deleted——recalled 占位可转发，记录为产品决策） |
| `web/{app,ws,api,render}.js` | `msg:recalled` 处理 + held-id 漏斗 ★ + recall 按钮 + 409→success + `recallMessage`（ws/api） |
| `web/package.json` | **B2 修复**：`"test"` 脚本列表追加 `render_recall.test.js` ★ |
| `web/*.test.js` + `web/render_recall.test.js` | 缺口 ③ 已补：ws/api/render recall 用例 ★ |

### 6.2 间接影响（须回归）

- 编辑/删除路径（`editable_message` 新拒绝分支）、`get_message`/历史读取（占位可见性）、搜索（`searchable_text=''` 不再命中）、RAG 嵌入（`embedding=NULL` 后 worker 不重嵌——`list_without_embedding` 要求 `searchable_text <> ''`，已验证）。
- outbox relay 全 match 点（`outbox.rs` 两处 + `event_outbox.rs`）、`run_bus_listener` 帧形状（`room_event_to_frame_json` 唯一收口）。
- **authz_lint**：新 handler 解析 `MessageId`（非 RoomId/WorkspaceId）不触发扫描；`recall_message` 走 service-enforced 风格（如需向 `SANCTIONED_GUARDS` 加说明——先跑 lint 确认）。

### 6.3 公共接口 / 事件 / 配置 / 部署 / 回滚

- 公共接口：新 REST 端点 + WS 帧（纯增量，旧客户端不受影响）；`Message` JSON 新增可空字段（向后兼容）。
- 事件：`RoomEvent` 新变体（仓库内全量枚举已改全；无外部消费者）。
- 配置：无。部署：build → migrate → **全节点升级后才启用 recall（DS-4 部署纪律）**。回滚：§2 Migration 回滚序列。

### 6.4 明确不做（克制）

不做撤回时限（Slack 式窗口）/撤回原因字段/撤回通知推送/管理端撤回/`message_recalls` 独立表/新 crate/新依赖/新定时器/新表。**记录为产品决策（非本批次代码）**：reactions 是否随撤回清除（Slack 清除——待产品确认；当前 UI 已抑制 react 行）；撤回按钮仅作者可见（web 无房角色上下文，admin/owner 走 WS/REST，与 delete 同款）；撤回 ≠ 擦除（`message_edits` 历史 + audit digest 对成员可见——合规 review 要求产品披露文案）；`recalled_by` 不入 GDPR 匿名化集（与审计保留立场一致，需决策记录）；撤回无限流（S4，超需求，记入 SRE 告警 backlog）。

---

## 7. 门禁缺陷修复清单（failing-test-first；第 1/2 轮已修复在树，第 3 轮 B1/B2 待修）

> 纪律（testing.md §2）：**每个修复先写能复现的失败测试 → 确认红 → 修复 → 转绿 → 回归**。下表“先红测试”均为现树中的回归测试（修复后绿）；§7.2 三缺口已关闭；**§7.4 第 3 轮 B1/B2 必须先写先红**。

### 7.1 五项门禁缺陷（已修复，测试在树）

| # | 缺陷 | 先红测试（现绿） | 修复（已核查落点） |
|---|---|---|---|
| 1 | **P1 系统编辑复活内容** | `system_edit_after_recall_is_fenced`：recall → `get_version` → `edit_outboxed_system(原文, cur_version, None, None)` → 断言 `Ok(None)`、blocks==占位、version 不变 | `events.rs` 两系统编辑路径（`edit_locked_outboxed_in_tx`:87 / `update_voice_transcript_outboxed`:192）在 `lock_message_in_tx` 行锁读后对 `recalled_at.is_some() → Ok(None)`——围栏在写行的唯一事务内，覆盖 unfurl/transcribe/webhook 全部调用方；DS-1 的 relay 静默丢弃随之消除（无复活 edit 则 version 不前进，pending Recalled 正常送达） |
| 2 | **P1/P2 replay 不送达撤回** | `changes_since_delivers_recalls`：`since=now-1min` 建消息+recall → `changes_since` 返回该行且 `recalled()` | `query.rs:180-181` 谓词/排序改 `GREATEST(edited_at, deleted_at, recalled_at)`；0238 重发 `idx_messages_room_mutated`（三列表达式，覆盖旧谓词行） |
| 3 | **P2 客户端 held-id 去重丢占位** | （web 缺口 ③ 已关闭：held-id replay 漏斗用例） | `app.js:219`：held 行带 `deleted_at/recalled_at/edited_at` → `applyChange` 守卫漏斗（tombstone → recall → edit 优先级），仅真重投去重 |
| 4 | **P2 SPA 无撤回入口** | （web 缺口 ③ 已关闭：ws/api/render 用例） | `ws.js:539 recallMessage`、`api.js:376 recallMessage`、`render.js` ↶ 按钮（作者）、`app.js` 处理器 + 409→success 映射（409 = 已生效，广播/回放收敛行） |
| 5 | **HIGH backfill 缺 recall 列** | `partition_backfill_carries_recall_columns`（`from_id` 游标限定，避开 blob 跨 scope 触发器） | 0238 `CREATE OR REPLACE FUNCTION backfill_messages_partition` INSERT/SELECT 双投影含 `recalled_at, recalled_by`（0148/0149/0158/0174 约定）；迁移级验证：影子 `recalled_at IS NULL WHERE live.recalled_at IS NOT NULL` 必须为 0 |

### 7.2 测试缺口（已全部关闭，均在树且绿）

| 缺口 | 测试 | 断言 |
|---|---|---|
| ① | `recalled_payload_is_delivered_at_version_and_suppressed_when_superseded`（`aero-im-core/src/service/outbox.rs`） | version 前进/行删/行消失 → `None`（suppressed）；version 相等 → `Recalled` 带当前占位（delivered） |
| ② | `concurrent_double_recall_has_exactly_one_winner`（`recall_tests.rs`） | `tokio::join!` 恰一 `Ok(Some)` 一稳定 409；version==orig+1；恰一条 outbox |
| ③ | `ws.test.js` ×2 + `api.test.js` ×2 + `render_recall.test.js` ×2 | 帧发送形状、409 ApiError、按钮可见性矩阵、`_lastSeen` 卫生 |

### 7.3 验收复核点（对门禁逐条）

- P1：`cargo test -p aero-storage --lib -- --ignored message::recall` 含 fence 用例绿；手工 probe 脚本（recall → edit_outboxed_system → `SELECT blocks`）确认占位未变。
- P2：`changes_since` 用例绿；web 端 `applyChange` recall 分支不再死代码（held-id 漏斗 + 三分支断言）。
- P3 客户端：`node --test web/` 全绿（82 用例，缺口 ③ 已补）。
- HIGH：fresh 库迁移 replay + backfill 函数调用后影子行列齐平（`partition_backfill_carries_recall_columns`）。
- 全量：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` · `scripts/{truth-check,file-size-check,web-check}.sh` · `scripts/test-integration.sh`（一次性库，`--test-threads=1` 避既有并发污染）。

### 7.4 门禁第 3 轮 B1/B2（**必须先写先红**）

| # | 先红测试 | 断言（现状红 → 修复后绿） | 修复落点 |
|---|---|---|---|
| B1 | 纯函数单测 `redact_blocks_for_recall_snapshot`（`mod.rs`，hermetic） | `File{blob_id}` → 文本块 `[附件已移除]`；`Voice{transcript: Some(t)}` → 文本块含 `t`；`Voice{transcript: None}` → `[语音已移除]`；其余块不变；输出无任何 `blob_id`（现状：函数不存在，编译红） | `mod.rs` 新增 pub(crate) 纯函数（挨着 `attached_blob_ids`） |
| B1 | storage 回归 `recall_snapshot_redacts_blob_references_and_gc_proceeds`（`recall_tests.rs`，PG） | 消息含 File+Voice 块 → 撤回 → ① `message_edits.blocks` JSON **不含**任一 blob_id 且含 transcript 文本；② `blob_gc_queue` 含该两 blob 行；③ 消息行 == 占位（现状：快照含 blob_id → 断言 ① 红） | `authorization.rs` 撤回事务：快照插值改 `serde_json::to_value(&redact_blocks_for_recall_snapshot(&existing.blocks))`；`blob_ids` 仍从原始 blocks 计算（GC 不变） |
| B2 | `cd web && npm test` | 测试列表含 `render_recall.test.js` 且全绿（现状：列表漏该文件 → `npm test` 不跑它；修复后 `npm test` 通过） | `web/package.json` `"test"` 脚本追加 `render_recall.test.js` |

B1 修复后 storage recall 应 **10/10**（9 既有 + 1 新回归），全量门重跑（含 `npm test`、`test-integration.sh`）。

---

## 8. 完成定义（DoD）

`cargo check --workspace` 干净 · `cargo test --workspace --lib` 全绿（2158）· PG 门控 recall 全套（**10/10** storage 含 B1 回归 + 28 im-core）`-- --ignored` 绿 · `cargo clippy --workspace --all-targets` 0 新增警告 · `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 · `node --test web/*.test.js` 全绿 · **`cd web && npm test` 全绿且含 `render_recall.test.js`（B2）** · `scripts/test-integration.sh` 绿（fresh 库 replay 含 0238）· authz_lint 6/6 · 门禁第 1/2 轮 5 缺陷逐条复核（§7.3）· **门禁第 3 轮 B1/B2 修复 + 回归（§7.4）**。

### 8.5 产物契约（上一轮 VALIDATION_FAILED 的修复——**实现者必须执行**）

1. **产物路径**：最终报告写入流水线输出路径 **`docs/pi-batch/feature-implementation.md`**（覆盖陈旧的首轮报告；该文件当前内容不代表本轮）。
2. **最终回复（即 artifact 文本）必须以 ```yaml 围栏的 `completion_report:` 块收尾**，结构照抄 completion-evidence.md 模板：

```yaml
completion_report:
  summary: ""
  changed_files: [ "migrations/0238_message_recall.sql", "crates/aero-storage/src/message/recall_tests.rs", ... ]  # 非空
  commands_executed:
    - command: "cd /home/u1/aero-im && cargo check --workspace --all-targets"
      result: passed                       # ∈ passed | failed | not_executed
    - command: "cd /home/u1/aero-im && cargo clippy --workspace --all-targets -- -D warnings"
      result: passed
    - command: "cd /home/u1/aero-im && cargo test --workspace --lib"
      result: passed
    - command: "cd /home/u1/aero-im && bash scripts/web-check.sh"
      result: passed
    - command: "cd /home/u1/aero-im && bash scripts/truth-check.sh"
      result: passed
    - command: "cd /home/u1/aero-im && bash scripts/test-integration.sh"
      result: passed
    - command: "node --test web/*.test.js"
      result: passed
  not_executed:
    - check: real-browser E2E recall UI flow
      reason: no browser harness in this environment; covered by frame contract + render tests + web-check
  residual_risks: [ ... ]
  assumptions: [ ... ]
```

3. **交付前本地验证**：`python /home/u1/ai-batch-runner/scripts/check-completion-report.py docs/pi-batch/feature-implementation.md` 必须输出 `COMPLETION: OK`（exit 0）。检查器硬规则：`result` 限 `{passed, failed, not_executed}`；`not_executed` 条目必须带 `reason`；`commands_executed`/`changed_files` 非空；禁止伪造通过措辞（`理论上应该通过`/`should pass`/`assume passed`）。散文式报告 = 自动 VALIDATION_FAILED。
4. **如实分级**：已运行并通过 → `passed`；已运行但失败 → `failed`（须附修复说明）；未运行 → 必须进 `not_executed` 并给原因。禁止把「理论上应该通过」写成 passed。
5. **不做**：不提交 git；不碰无关文件；不重写已绿代码（本轮实现已在树，重点是重验 + 合规产物）。

**明确不满足即不宣布完成**：撤回内容对离线/重连客户端保持「已消失」（缺陷 2/3 收敛）；系统编辑不可复活占位（缺陷 1）；cutover backfill 不丢撤回状态（缺陷 5）；SPA 可发起撤回（缺陷 4）。
