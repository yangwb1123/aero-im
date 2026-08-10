# Requirements Spec — aero-live-core：L1 聚合策略钉定（高容量 live/chat 通道 → governance outbox）+ churn-priority drill 扩展

- **Module (analysis root)**: `crates/aero-live-core` — 但本 direction 的落地面横跨 `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（drill 扩展）、`crates/aero-storage/src/audit_governance.rs`（db_test）、`scripts/b5-pin.sh` + `scripts/test-integration.sh`（pin 槽位）与 `crates/aero-ai/src/governance.rs`（映射权威单测 pin）
- **Direction**: "Pin the L1 aggregation policy for high-volume live/chat lanes feeding the governance outbox (the only [PROPOSED] B5-1 deliverable with zero DDL), and extend the priority drill to prove moderation preempts backlog under volume"（value 9 / risk_reduction 7 / effort 6 / confidence 7）
- **Source analysis**: `docs/auto/analyses/crates-aero-live-core-d5bccc8d.json`（direction #0，proposed）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md:8`（B5-1 DDL、P2 parity event_id 1:1、「‡ 类走 L1」）；gate anchor `docs/campaigns/implementation-gate.md:64`（"`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1"）、:78（G6 = 37/37、T-11、moderation 优先级）
- **Status**: Requirements（下述证据全部经源码 grep 核对，行号 = 2026-08-09 核对锚点，可能漂移——文件/符号才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-09

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-common/src/model/audit.rs:165`（L1-aggregatable class） | ✅ **Verified（精确）**。:165 = `/// High-volume message backlog class (L1-aggregatable).`，:166 = `pub const GOVERNANCE_CLASS_MESSAGE: &str = AuditClass::Message.as_str();`（:122-126 `AuditClass { Message, Room, Admin }`）。**"L1-aggregatable" 是叶子对 message class 的显式指定**——本 direction 钉定的就是这句话的语义。 |
| E2 | `crates/aero-ai/src/governance.rs:31/:33/:60`（GOVERNANCE_CLASS_*/PRIORITY_BACKLOG/MODERATION_OUTBOUND_ACTION） | ✅ **Verified（行号微移）**。`GOVERNANCE_PRIORITY_MODERATION: i16 = 100` :31、`GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` :33（= 0239 列 DEFAULT）；:41 `pub use aero_common::model::audit::{GOVERNANCE_CLASS_*, LOCAL_ACTION_MODERATED, MODERATION_OUTBOUND_ACTION}` 再导出链；`governance_lane_for` :74-88 唯一映射（`message.moderated` → class admin / priority 100 / status 0，未知 token → `None`）；`is_admin_class` :90。单测 `unknown_local_token_passes_through_unmapped`（`message.create`/`message.deleted`/`room.create` 等全 `None`）与 `admin_class_rows_never_aggregated`（:200，**admin class 绝不进 [PROPOSED] L1 聚合窗**）。 |
| E3 | `StreamEvent::Chat` at `crates/aero-common/src/live.rs:119` | ✅ **Verified（行号微移）**。`StreamEvent`（`#[serde(tag="kind")]`）枚举起 :118，`Chat(StreamChatLine)` :121；`StreamChatLine` :66-79（id/stream_id/sender_id/body/created_at——**已是持久模型**）。E12 见下。 |
| E4 | `migrations/0240_audit_governance_due_prio_idx.sql`（priority DESC claim ORDER BY） | ✅ **Verified**。`CREATE INDEX IF NOT EXISTS audit_governance_due_prio_idx ON audit_governance_outbox (priority DESC, available_at, created_at, event_id) WHERE status IN (0, 1)`——与 claim ORDER BY 逐项匹配（LIMIT pushdown）。0240 文档明言 legacy FIFO 索引（0239 的 `audit_governance_due_idx`）**保留**共存（rolling-deploy D5）。 |
| E5 | `crates/aero-audit-connector/src/pg.rs:117`（claim ORDER BY priority DESC）与 :520（mixed_priority_claim_orders_moderation_first_then_fifo） | ✅ **Verified（行号微移）**。`claim_due` 的 CTE `ORDER BY candidate.priority DESC, candidate.available_at, candidate.created_at, candidate.event_id … FOR UPDATE SKIP LOCKED LIMIT $1` :116-127；db_test `mixed_priority_claim_orders_moderation_first_then_fifo` :546（40 backlog priority 10 + 10 admin priority 100、**inverted created_at** 证明非 FIFO、`limit 25 < 50`、claimed set = 全部 admin ∪ 最早 15 条 backlog——set 契约 D3）。 |
| E6 | `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（existing drill to extend） | ✅ **Verified**。`BACKLOG_ROWS=500`（priority 10、class 'message'、**先入**）、`MODERATION_ROWS=1`（class 'admin'、priority 100、action = `MODERATION_OUTBOUND_ACTION`、**后入**）、`BATCH_SIZE=100`（首批容不下全部）、`MAX_ROUNDS=10`、`concurrency=1` + stub sink；断言 = round-1 恰 100 行且 moderation 行 IN 该集合（**D3：batch 成员资格是契约，delivery firstness 是 executor artifact 永不断言**）+ 全排空 501 行 set-parity + D8′ 词汇 gate（leaf 翻转不 auto-follow）+ TRUNCATE 破坏性 gate（`AERO_PRIORITY_DRILL_ALLOW_TRUNCATE`）。0239 表缺席 → exit 2 SKIP。 |
| E7 | `scripts/b5-pin.sh:46-67`（22 [PROPOSED] slots） | ✅ **Verified（精确）**。`B5_CONTRACT_TEST_LIST` = 15 执行槽（`audit_governance::`、`moderation-priority-drill`、`t11-fail-closed`、`moderation_finalize_outbox_parity` 等）+ :46-67 恰 **22** 个 `contract-test-NN[PROPOSED]` = 37/37。guard（:85-133）：恰 37 槽、槽名正则 `^[A-Za-z0-9_:+-]+(\[PROPOSED\])?$`、无重复、≥1 执行槽（禁 vacuous green）、fresh 模式每个执行槽须有 `B5-CHECK <name>: PASS|SKIP` 证据行。 |
| E8 | 0239 触发器（唯一非 moderation 入队门） | ✅ **Verified**。`migrations/0239_audit_governance_outbox.sql`：`event_id UUID PRIMARY KEY`（= audit_events.id 1:1，:29）、token-keyed 触发器 `aero_enqueue_governance_audit()` **仅 `NEW.action = 'message.moderated'` 入队**（class 'admin'/priority 100/status 0），未知 token `RETURN NEW` fail-open pass-through（:83-84）；`class IN ('admin','message','room')` + `priority > 0`（DEFAULT 10）+ `delivery_mode IN ('push')` + status CHECK 0..3。**chat churn 行今天不可能经触发器入 outbox——本 direction 的「0 行」断言钉的就是这个现状**。 |
| E9 | `crates/aero-storage/src/audit_governance.rs`（db_tests，harness filter `audit_governance::`） | ✅ **Verified**。模块 doc 明言：filter `audit_governance::` = 本模块 db_tests（harness 空过滤器守卫禁 vacuous green）；「The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a sibling slice … **add the repo to this same module when it lands**」。既有测试：`moderation_finalize_outbox_parity`（**event_id 1:1 with audit_events.id（P2 parity）**、rollback/replay 半部）、`ddl_contract_defaults_and_checks`（DEFAULT 10/class 'message' 行为断言）、`non_moderation_action_passes_through_unmapped`（unmapped token → 0 outbox 行 + v1 行照常）。helper：`fixture`/`enable_enforcement_with_binding`/`moderate_finalize`（真实生产 seam `soft_delete_outboxed_system`）/`reset_governance_table`/`restore_enforcement_disabled`——**新 db_test 复用全部 helper，零新 seam**。 |
| E10 | `AuditRepo::append_in_tx`（chat 审计行的真实 in-tx 生产者） | ✅ **Verified**。`crates/aero-storage/src/audit.rs:127`：`append_in_tx(tx, workspace, actor, action, target, detail) -> Result<AuditId>`——audit INSERT 骑调用者事务；`audit_governance.rs` Half B 已用同一 seam。`audit_events` INSERT 会触发 0239 触发器（token-keyed pass-through）。 |
| E11 | L1 聚合的既有契约词汇 | ✅ **Verified**。`docs/campaigns/implementation-gate.md:64` 行 1 = "‡ 类走 L1"（高容量事件豁免 1:1 parity）；auth 切片 `docs/requirements/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.req.md` **R8**（[PROPOSED] 形态参考）：聚合窗 = `workspace × action × 窗口`（默认 60s），N 事件 → **恰 1 行**（payload 带 `count`），**AC3** 断言形态 "N 事件 → 1 行"；D2 结局 A：失败事件 class='message' 入 L1 窗（与 `admin_class_rows_never_aggregated` 自洽）。 |
| E12 | live churn 的持久化底座 + 总线性质 | ✅ **Verified**。`migrations/0005_p4_live_interactive.sql:7` `stream_chat`、:19 `stream_gifts`（逐条持久，`idx_stream_chat_stream_time`）——**danmaku 已有独立持久表**，进 outbox 是纯重复；AGENTS §1：`live.stream.*` = **ephemeral** consumer（丢几条弹幕无碍），无 durable cursor——churn 的 durable 入队需要新游标契约（范围外）。 |
| E13 | T-11 fail-closed drill（验收引用的姿态锚） | ✅ **Verified**。`crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`：relay 缺席（token endpoint 确定性关闭）⇒ 行 stay pending（status 0、零终态、attempts 逐轮 +1、last_error 记录传输失败）——**永不静默投递、永不假死**；slot `t11-fail-closed` 已执行。本 direction 的 drill 扩展保住同一姿态：全排空后 0 stuck（status 1）+ 0 dead（status 3）。 |

### 1.1 对 direction 问题陈述的勘误/注解（evidence-backed）

- **「no policy decides how live danmaku/chat churn … maps into the outbox」成立，且有一个被 direction 低估的既有事实**：0239 触发器是 token-keyed（E8），`governance_lane_for` 是唯一映射权威（E2）——**今天 churn 入 outbox 的路根本不存在**（不是「未决定」，是「无生产者 + 无映射」）。因此本 direction 钉的不是「在两条路里选一条」，而是**钉死现状为策略**（lifecycle-only，R1），并同时钉死「若未来 message-class 生产者落地，窗口聚合契约的上限」（≤1/window，R2）——两个分支都在 §7 D1 决策记录里显式落案。
- **「thousands of unmapped churn rows would starve B5-3 moderation priority」的机制陈述准确，但前提是「churn 行已经入 outbox」**：0240 due-prio 索引 + `claim_due` 的 `priority DESC`（E4/E5）使 priority 10 积压在 priority 100 之前 FIFO 排空；`mixed_priority_claim_orders_moderation_first_then_fifo`（E5）已钉死 preemption——**B5-3 的 claim 侧机制完整**，缺的正是 producer 侧「什么进 outbox」的策略。本 direction 的 drill 扩展把 500 积压行**重塑为 chat-churn 形状**（class 'message'/priority 10/payload action 'live.chat'），证明「即使 churn 大规模入表，admin.content.flag 仍在首批被 claim」——把 mechanism 证据从「generic backlog」升级为「本 direction 钉定的那条 lane」。
- **「22/37 b5-pin.sh slots remain [PROPOSED]」精确**（E7）：替换**一个**占位槽为执行槽后 = 16 执行 + 21 [PROPOSED]，总数仍 37。
- **验收的 "or" 分支**（`≤1` windowed-dedup / `0` lifecycle-only）：本 spec 选择 **lifecycle-only（0）** 为 live lane 的落地策略（§7 D1，理由 E8/E12：无生产者、无审计行、已有持久表、ephemeral 总线）；windowed-dedup（≤1）作为 **message class 的保留类契约**钉在 R2（不建聚合器——零代码零 DDL，形态引用 auth R8）。db_test 断言 chosen 分支（恰 0），R2 的 ≤1 以「类契约 + 单测 pin」落地，不制造无生产者的假聚合测试。

## 2. Verified current state

```
audit_governance_outbox（0239 已落工作树）：
  event_id PK = audit_events.id（1:1，P2 parity）· status 0/1/2/3 · class admin|message|room
  · priority > 0（DEFAULT 10 = BACKLOG；moderation 100）· payload JSONB object
  · 0240 due-prio 部分索引 (priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)

入队现状（唯一路）：
  audit_events AFTER INSERT → aero_enqueue_governance_audit()
  └─ 仅 NEW.action='message.moderated' → outbox（class admin/priority 100）；其余 RETURN NEW pass-through（0 行）

live/chat 高容量通道现状（E3/E8/E12）：
  StreamEvent::Chat（live.stream.* ephemeral bus）→ stream_chat 表持久（0005），无 audit_events 行
  StreamEvent::Status（生命周期）→ stream_go_live_outbox（golive 通知，非治理 outbox）；审计映射 = sibling spec 条件契约
  治理 outbox 里今天不存在任何 live-lane 行——「0 行」不是缺实现，是 token-keyed pass-through 的现状

claim 侧（B5-3，E4/E5）：
  claim_due: ORDER BY priority DESC, available_at, created_at, event_id LIMIT n FOR UPDATE SKIP LOCKED
  mixed_priority db_test：40 backlog(10) + 10 admin(100) → claimed set = 全部 admin ∪ 最早 15 backlog（D3 set 契约）
  aero-audit-priority-drill：500 backlog + 1 moderation，moderation IN 首批（成员资格契约），全排空 501 set-parity

pin 现状（E7）：37 槽 = 15 执行 + 22 [PROPOSED]；guard 禁 vacuous green、逐槽 verdict 证据
```

## 3. Scope

**In scope**（全部零 DDL、零新 crate、零新表）：
1. **策略钉定**：live/chat 高容量 churn → governance outbox 的映射策略（R1 lifecycle-only + R2 类级窗口契约），落案为决策记录 + 映射权威单测 pin。
2. **drill 扩展**：`aero-audit-priority-drill.rs` 的积压人群重塑为 chat-churn 形状（K 行 class 'message'/priority 10）+ 1 行 `admin.content.flag`（class 'admin'/priority 100），断言保持不变形（首批成员资格 + 全排空）+ 新增「churn 在 admin 之后才排空」的契约诚实编码。
3. **pin 槽位**：b5-pin.sh 替换一个 [PROPOSED] 占位为执行槽 `chat-churn-priority-drill`（37 总数不变），harness 发 verdict 行。
4. **db_test**：`audit_governance::` filter 下新增「N 条 chat churn 行（一个 L1 窗内）→ 恰 0 outbox 行；admin 行 1:1」的 db_test。

**Out of scope**（明确不建）：
- L1 聚合器本体（窗口扫描、聚合行写入、payload `count`）——R2 只钉契约形态（≤1/window），落地属未来 message-class 生产者方向（auth R8 同族），本 direction **零代码**。
- `IngestEvent` 接线 / lifecycle 审计生产者——sibling spec `2026-08-08-aero-live-core-ingestevent-lifecycle-producer.req.md`（R1/R3 条件契约），本 spec 只引用其形状。
- 任何迁移、任何 connector 行为改动（pg/relay/client/outbox 零改动，E13 的 t11 drill 零改动）。

## 4. Requirements

### R1 — 策略钉定：live/chat churn 走 lifecycle-only（恰 0 行）

`StreamEvent::Chat`（danmaku）、`Gift`、`Viewers`、`HypeTrain` 高容量 churn **永不进入 `audit_governance_outbox`**。依据（E8/E12）：① churn 无 `audit_events` 行——outbox `event_id` PK = `audit_events.id` 1:1 是 P2 parity 契约（contract :8），无审计行即无 1:1 锚；② churn 已逐条持久于 `stream_chat`/`stream_gifts`（0005）——入 outbox 是纯重复；③ `live.stream.*` 是 ephemeral 总线（无 durable cursor）——durable 入队需新游标契约，范围外。**生命周期转换（`Status{Live}`/`Status{Ended}`）是唯一的自然低容量 1:1 行**，其形状由 sibling spec R3 条件契约钉定（本 direction 不重复定义）。

**映射权威单测 pin**（`crates/aero-ai/src/governance.rs`，零新函数）：
- `unknown_local_token_passes_through_unmapped` 的 token 列表追加 `"live.chat"`（及 `"live.gift"`）：`governance_lane_for("live.chat") == None`、`!is_admin_class("live.chat")`——**churn token 永不进入 admin lane（priority 100），也永不映射任何 lane**。这是「恰 0 行」在映射权威层的编码：`None` → 0239 触发器 pass-through（E8 :83-84）→ 0 outbox 行。
- 不改 `admin_class_rows_never_aggregated`（R5 既有 pin 保持原义）。

### R2 — 类级 L1 窗口契约（保留契约，不建聚合器）

「L1-aggregatable」（E1）的语义钉定为：**若未来任何 message-class 生产者（如 room message 积压）把 churn 路由进 outbox，契约上限 = 每 (workspace × action × 60s 窗口) ≤1 行**（windowed-dedup；形态引用 auth R8：payload 带 `count` = N）。约束：
- **admin class 永不聚合**（E2 `admin_class_rows_never_aggregated`）——1:1 parity 是 admin lane 的硬契约。
- 窗口是 **producer-side 契约**，不由 DDL 强制（0239 无窗口列/约束——`ddl_contract_defaults_and_checks` 已钉 DDL 形状，不新增约束）。
- 本 direction **不实现聚合器**（零代码）；R2 是文字契约 + 决策记录（§7 D1），供未来生产者方向引用。

### R3 — drill 扩展：chat-churn 积压人群 + admin.content.flag 首批 claim

扩展 `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（**同一个 bin**，E6 全部既有机制保留）：
- 积压人群重塑为 chat-churn 形状：`BACKLOG_ROWS`（= K = 500）行全部 class `'message'`、priority `BACKLOG_PRIORITY`（= 10，生产值 verbatim）、payload 增加 `"action": "live.chat"` 标记（代表 R1 钉定的 live lane 若入表时的行形状）；`MODERATION_ROWS` = 1 行 class `'admin'`、priority `MODERATION_PRIORITY`（= 100）、action = `MODERATION_OUTBOUND_ACTION`（`admin.content.flag`）。先入 backlog、后入 moderation（ordering 证据只能来自 priority）。
- 断言（全部既有，语义不变形）：
  - round 1：`claimed == BATCH_SIZE`（100 < 501）且 admin 行 `delivered_at IS NOT NULL`——**首批成员资格**（D3 set 契约，不断言 intra-batch firstness）。
  - **新增**「churn 在 admin 之后才排空」（契约诚实编码）：round 1 完成时 `COUNT(status=2 WHERE class='admin') == 1` 且 `COUNT(status IN (0,1) WHERE class='message') == 401`（= 501 − 100）——admin 已投递而 churn 积压仍在排空队列，**preemption 的批量级证据**。
  - 全排空：`COUNT(status=2) == TOTAL_ROWS`（501）且 stuck（status IN (0,1,3)）== 0——**T-11 姿态**：0 假死、0 卡死（E13），任何 churn 行不丢不假死。
  - D8′ 词汇 gate、TRUNCATE 破坏性 gate（`AERO_PRIORITY_DRILL_ALLOW_TRUNCATE`）原样保留。
- 输出新增一行机器可 grep 的 PASS 线：`drill: chat-churn-lane: PASS`。

### R4 — pin 槽位替换 + harness verdict

`scripts/b5-pin.sh`：`contract-test-01[PROPOSED]`（:46 第一个占位）替换为执行槽 `chat-churn-priority-drill`——37 总数不变（16 执行 + 21 [PROPOSED]），槽名过正则 `^[A-Za-z0-9_:+-]+$`。`scripts/test-integration.sh` 的 moderation-priority drill 段（:503-577 先例）：drill 运行成功后 grep `drill: chat-churn-lane: PASS` 并 `b5_check "chat-churn-priority-drill" "PASS"`（0239 缺席时 `SKIP (0239 not landed)` 同款分支）。既有 `moderation-priority-drill` 槽的 verdict 从同一段继续发出（同一 drill 运行产出两条 verdict 线）。

### R5 — db_test：N 条 churn 行（一个 L1 窗内）→ 恰 0 行；admin 行 1:1

`crates/aero-storage/src/audit_governance.rs` db_tests 模块新增一个测试（`#[tokio::test] #[ignore = "requires live Postgres"]`，filter `audit_governance::` 自动匹配，harness 空过滤器守卫满足）：
- 前置：`reset_governance_table` + `fixture`（ws/actor）+ `enable_enforcement_with_binding`——**0239 两道 gate 全开**，证明「0 行」是 token-keyed pass-through（E8），不是 gate 关闭的伪证据（`non_moderation_action_passes_through_unmapped` 先例）。
- 窗内 churn：**单事务**内 `AuditRepo::append_in_tx`（E10，真实生产 seam）× N=3，action `"live.chat"`、target = 流/消息 id、detail 带 `stream_id`/`body`——同一窗口（同事务同秒）。
- 断言 A（lifecycle-only）：`audit_events` 中 action='live.chat' 恰 3 行（churn 被本地审计），`COUNT(audit_governance_outbox) == 0`——**N 条 churn 行在一个 L1 窗内 → 恰 0 行**（验收 lifecycle-only 分支）。
- 断言 B（P2 parity）：同一 ws 内 `moderate_finalize`（真实生产 seam）1 条 → 恰 1 outbox 行，`event_id == audit_events.id`（join 断言，镜像 `moderation_finalize_outbox_parity` Half 3 形态）、class 'admin'、priority 100、status 0——**admin-class 行 1:1**。
- 收尾：`restore_enforcement_disabled`（全局 singleton，先例同款）。

## 5. Acceptance（验收原文逐条保留 + 可测化）

> **Acceptance 原文**：Replace one [PROPOSED] b5-pin.sh slot with an executed drill: enqueue 1 admin.content.flag (class admin, priority 100) + K chat-churn backlog rows (priority 10) → relay claims the admin row first and drains churn only after (B5-3 moderation priority preserved, T-11). db_test (audit_governance:: filter): N chat rows within one L1 window produce ≤1 outbox row (windowed-dedup policy) or exactly 0 (lifecycle-only policy), while each admin-class row stays 1:1 with audit_events.id (P2 parity)

| # | 验收原文 | 可测化断言（本 spec 的 chosen 分支加粗） |
|---|---|---|
| **AC1** | Replace one [PROPOSED] b5-pin.sh slot with an executed drill | `scripts/b5-pin.sh` 37/37 guard PASS（16 执行 + 21 [PROPOSED]，无重复/无 malformed/非 vacuous）；执行槽 `chat-churn-priority-drill` 在 `$B5_LOG` 有 `B5-CHECK chat-churn-priority-drill: PASS` 证据行；`scripts/test-b5-pin-guard.sh` 绿。**drill 输出含 `drill: chat-churn-lane: PASS`** |
| **AC2** | enqueue 1 admin.content.flag (class admin, priority 100) + K chat-churn backlog rows (priority 10) | drill 种子断言（可 grep seed 日志或 psql 复核）：`COUNT(class='admin' AND priority=100 AND payload->>'action'='admin.content.flag') == 1` 且 `COUNT(class='message' AND priority=10 AND payload->>'action'='live.chat') == K`（K = 500），admin 行后入（`created_at` 晚于全部 churn 行） |
| **AC3** | relay claims the admin row first and drains churn only after (B5-3 moderation priority preserved) | round 1：`claimed == 100`；admin 行 `delivered_at IS NOT NULL`（**首批成员资格**，D3 契约）；round 1 完成时 `COUNT(status=2 WHERE class='admin') == 1` 且 `COUNT(status IN (0,1) WHERE class='message') == K−99`——admin 已投递、churn 仍在排空（preemption 的批量级证据）；全排空 `COUNT(status=2) == K+1` |
| **AC4** | T-11 | 全排空后 `COUNT(status=3) == 0`（0 假死）且 `COUNT(status=1) == 0`（0 卡死）——T-11 姿态在 volume 下保持；`t11-fail-closed` 槽及其 drill 零改动（§8） |
| **AC5** | db_test (audit_governance:: filter): N chat rows within one L1 window produce **exactly 0** outbox row (**lifecycle-only policy**, chosen) | `cargo test -p aero-storage --lib "audit_governance::" -- --ignored --test-threads=1` 在 throwaway migrated DB 上绿且 ≥1 测试实跑（harness 空过滤器守卫）：N=3 条 `live.chat` 审计行（单事务单窗口、gate 全开）→ `COUNT(audit_governance_outbox) == 0`；`≤1`（windowed-dedup）作为 message-class 保留类契约钉于 R2/§7 D1（不建聚合器，无假聚合断言） |
| **AC6** | each admin-class row stays 1:1 with audit_events.id (P2 parity) | 同一 db_test：1 条 `message.moderated`（真实 in-tx seam）→ 恰 1 outbox 行且 `event_id == audit_events.id`（join 断言）、class 'admin'、priority 100、status 0——P2 parity 与既有 `moderation_finalize_outbox_parity` 一致 |

## 6. Landing points（文件清单，零新 crate）

| 文件 | 改动 |
|---|---|
| `crates/aero-ai/src/governance.rs` | 单测 `unknown_local_token_passes_through_unmapped` token 列表追加 `"live.chat"`/`"live.gift"`（R1 pin，零新函数） |
| `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs` | 积压人群 payload 加 `"action": "live.chat"` 标记 + 注释钉 R1/R2；新增 round-1 后 churn-pending 计数断言与 `drill: chat-churn-lane: PASS` 输出（R3；`BACKLOG_ROWS`/`BATCH_SIZE`/`MAX_ROUNDS`/gate 原样） |
| `scripts/b5-pin.sh` | `contract-test-01[PROPOSED]` → `chat-churn-priority-drill`（R4；37 总数不变，guard 自动要求 verdict） |
| `scripts/test-integration.sh` | moderation-priority drill 段追加 grep `drill: chat-churn-lane: PASS` + `b5_check "chat-churn-priority-drill" "PASS"`（R4；0239 缺席 SKIP 分支同款） |
| `crates/aero-storage/src/audit_governance.rs` | 新 db_test（R5；复用 `fixture`/`enable_enforcement_with_binding`/`moderate_finalize`/`reset_governance_table`/`restore_enforcement_disabled` + `AuditRepo::append_in_tx`） |

## 7. Decisions

### D1 — L1 聚合策略（本 direction 的核心决策，落案即 pin）

- **Tier 1（live lane churn，落地）**：lifecycle-only——`Chat`/`Gift`/`Viewers`/`HypeTrain` churn 恰 0 行（R1）。理由：无 audit_events 行（P2 parity 锚缺失，E8）、已有持久表（E12，0005）、ephemeral 总线无 durable cursor（E12）。0 行 = token-keyed pass-through 的现状（`governance_lane_for("live.chat") == None`），单测 pin 后是**钉死的策略而非未决状态**。
- **Tier 2（message class，保留契约）**：windowed-dedup——若未来 message-class 生产者落地，上限 ≤1 行/(workspace × action × 60s 窗)，payload `count`（R2，形态引用 auth R8）。**不建聚合器**（零代码零 DDL）；`admin_class_rows_never_aggregated` 约束原样（admin 永不聚合）。
- **验收 "or" 分支的落案**：chosen = lifecycle-only（AC5 断言恰 0）；windowed-dedup 分支以类契约 + 决策记录钉住，**不制造无生产者的假聚合测试**（honest：没有聚合器就没有可断言的 ≤1 行为）。

### D2 — drill 扩展形态

- 扩展**现有 bin**（`aero-audit-priority-drill.rs`）而非新 bin：sibling spec（IngestEvent）已确立「extend aero-audit-priority-drill」惯例；同一 drill 运行产出 `moderation-priority-drill` 与 `chat-churn-priority-drill` 两条 verdict，机制零重复。
- 「claims the admin row first」的断言语言 = **D3 成员资格契约**（首批成员 + round-1 完成时 admin 已投递而 churn 未排空），**不**断言 `delivered_at` 的 intra-batch 次序（executor artifact，既有 drill 注释明言永不断言）。

### D3 — K 的取值

K = 500（既有 `BACKLOG_ROWS`，production 值语义不变）：`BATCH_SIZE = 100 < K + 1`，首批不可能容下全部——ordering 证据只能来自 priority（与既有 drill 同构）。

## 8. Zero-change list（零改动清单）

`migrations/0239/0240/0241`、0239 触发器、`aero-audit-connector/src/{pg,relay,client,outbox,config,fake,stub}.rs`、`aero-audit-t11-drill.rs`、`aero-audit-relay-drill.rs`、`aero-eng/audit_provision.rs`、`aero-server`（boot/ingest、stream_live_outbox.rs、golive_bot.rs）、`aero-common`（`StreamEvent`/`StreamChatLine`/`AuditClass`/governance 常量——`"live.chat"` 是测试 token，**不加**任何叶子常量）、`stream_chat`/`stream_gifts`（0005）、`IngestEvent`（sibling spec 属主）、`moderation_finalize_outbox_parity` 等既有 db_tests、`t11-fail-closed`/`moderation-priority-drill`/`a3-relay-drill` 既有槽位语义。

## 9. 风险与残留 [PROPOSED]

- **「L1 聚合器本体」仍 [PROPOSED]**：R2 钉的是契约上限，聚合实现（窗口扫描、`count` 载荷）留待未来 message-class 生产者方向（auth R8 同族）——本 direction 不宣称完成它。
- **chat churn 的审计可见性**：lifecycle-only 意味着 danmaku 只存在于 `stream_chat`（0005），不进审计 sink——这是**策略选择**（churn 非治理事件，治理面 = 生命周期 + moderation），不是缺口；若产品要求 danmaku 治理审计，走 R2 窗口契约落地（新方向）。
- **drill 的「chat-churn 形状」是模拟**：drill 直接种子 outbox 行（既有 drill 先例），不经过真实 producer——因为 producer 不存在（R1 恰 0 行）。它证明的是 claim 侧在 churn 体积下的 preemption（B5-3），与 producer 侧策略（R1/R5 db_test）正交。
