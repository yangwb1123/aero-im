# Requirements Spec — B5-1: 落库 0239 `audit_governance_outbox`（status 0/1/2/3）+ `audit_events` 同事务 enqueue —— 全部 B5 产物的唯一阻塞项

- **Module (analysis root)**: `crates/aero-ai`（governance.rs 为映射权威 + 验收 oracles；DDL 落在 `migrations/`，与 sibling aero-storage slice 共享）
- **Direction**: "Land migration 0239: audit_governance_outbox DDL (status 0/1/2/3) + in-tx enqueue from audit_events — the single blocker gating every B5 artifact"（value 10 / risk_reduction 10 / effort 6 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-f8cd3622.json` (direction #1)
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（v2 契约正文在仓外，[PROPOSED]）
- **Status**: Requirements（下述证据全部经源码 grep 核对；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-07

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `migrations/` 尾号 0238，无 0239 | ✅ **Verified**。`ls migrations/*.sql \| wc -l` = **238**；尾两件 = `0237_snaplink_destination_credentials.sql` + `0238_message_recall.sql`。`ls migrations/0239*` 空、`git log --all -- migrations/0239*` 空——**0239 从未存在**。文件名被 in-repo 消费者钉死：`scripts/test-integration.sh:306,325,347,447` 全部 `[ -f "migrations/0239_audit_governance_outbox.sql" ]` 门控（else 分支显式 `b5_check … SKIP (0239 not landed)`）；`scripts/b5-pin.sh:37-38` 命名条目 `audit_governance::` + `moderation_finalize_outbox_parity`。 |
| E2 | `crates/aero-ai/src/governance.rs:32,58-69,84`（0239 列默认、enqueue 重定向、status 0、priority 10） | ✅ **Verified**（行号微漂：实际 31/33/61-74/71/86-94）。`GOVERNANCE_PRIORITY_MODERATION: i16 = 100`（:31）、`GOVERNANCE_PRIORITY_BACKLOG: i16 = 10`（:33，doc 明言 **"also the 0239 column default"**）、`GovernanceLane`（:61-74，class/priority/outbound_action/status 四字段）、`governance_lane_for`（:86-94：`message.moderated` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`；未知 token → `None` pass-through）。6 项单测钉死契约：`moderation_lane_preempts_backlog_under_desc_claim` / `unknown_local_token_passes_through_unmapped`（含 `"message.deleted"`、`""`）/ `user_delete_token_stays_out_of_admin_lane`（R-D2 负钉）/ `mapping_is_token_keyed` / `outbound_action_is_single_contract_token` / `admin_class_rows_never_aggregated`。⚠️ 该文件当前 **untracked**（上一批次产物未提交）——本 direction 落地须先提交（§7 风险）。 |
| E3 | 三个 drill 的 SKIP 探测（`aero-audit-t11-drill.rs:73` / `relay-drill.rs:57` / `priority-drill.rs:87-116`） | ✅ **Verified**。t11-drill:73-83 与 relay-drill:57-67：`SELECT to_regclass('audit_governance_outbox')::text` 为 NULL → `eprintln!(SKIP …)` + `std::process::exit(2)`。priority-drill:87-97 同款表探测 + :105-118 `information_schema.columns` 探测 `priority`/`class` 两列（缺列同样 exit 2 SKIP）。**关键语义**（priority-drill:29-33 doc）：0239 落库但 B5-3 排序未落 → **FAIL 红而非 SKIP**（诚实 G6 信号）。drill 断言即验收 oracle：t11 = status 0 全量 re-park + 零终态 + `SUM(attempts)` 逐轮 N/2N + `last_error LIKE '%audit connector HTTP transport failed%'` 全行；relay = `COUNT(status=2)==N` + event_id 集合 parity；priority = round1 批 100 内 moderation 行 `delivered_at == MIN(delivered_at)` → 全 drain 501 + parity + moderation `delivered_at` 严格早于全部 backlog。**t11/relay drill 的 INSERT 不含 priority/class 列** ⇒ 两列必须带默认值。 |
| E4 | `crates/aero-audit-connector/src/pg.rs:69-120`（claim_due SQL 指名表/列） | ✅ **Verified**。`STATUS_ENQUEUED=0/CLAIMED=1/DELIVERED=2/DEAD=3` 常量 :26-30（doc：**"B5-1 0239 DDL normative values"**）。claim_due :69-108（`FROM audit_governance_outbox`、`status IN (0,1)`、`available_at <= clock_timestamp()`、`(lease_expires_at IS NULL OR <= clock_timestamp())`、`ORDER BY (available_at, created_at, event_id)`、`FOR UPDATE SKIP LOCKED`、`LIMIT` clamp `[1,500]`；RETURNING event_id/attempts/claim_token/lease_expires_at/payload）。settle :110-153（单事务 fenced 重读 → `status=2` + `delivered_at=clock_timestamp()` + 清 token/lease/last_error）；requeue :155-187（`status=0` + `clock_timestamp()+backoff` + last_error）；mark_dead :189-217（`status=3`）。测试 `ensure_outbox_table` :244-270 自建**最小等效形（无 priority/class）**——真实 0239 须含这两列（E3 探测）。**claim SQL 现无 priority 排序项**（`ORDER BY available_at, created_at, event_id`）——B5-3 方向的事，本 direction 不碰（§3）。 |
| E5 | `worker/mod.rs:401` + `messages.rs:632`（同事务 producer，`LOCAL_ACTION_MODERATED`） | ✅ **Verified**。`AiWorker::handle_moderate` :372-433，BLOCK 分支 :397 调 `soft_delete_outboxed_system(id, Some(workspace), None, Some(LOCAL_ACTION_MODERATED), detail, ParticipantId::nil(), None)`（常量 import :58；:401 = `Some(LOCAL_ACTION_MODERATED)` 参数位）；`workspace=None` 时 R-D1 **拒绝删除**（:389 前置 `moderation_delete_workspace(job)?`，失败 :422 `return Err(e.into())` → 重试 → DLQ）。`ImService::moderate_delete` :620-644 同路径，audit_action = `workspace.map(|_| "message.moderated")`（:636）。落库点 `soft_delete_outboxed_system` = `crates/aero-storage/src/message/events.rs:267`；`soft_delete_locked_outboxed_in_tx` 内 `AuditRepo::append_in_tx`（events.rs:337 → audit.rs:127）与软删 UPDATE + `RoomEvent::Deleted` event outbox **同一 PG 事务**。其余同 token producer（token-keyed 自动覆盖，无需改动）：`message/crud.rs:398 soft_delete_moderated`、`message_reports.rs:305 review_authorized`。 |
| E6 | `migrations/0236_snaplink_governance_reconciliation.sql:129-131`（AFTER INSERT trigger 先例） | ✅ **Verified**。:129 `DROP TRIGGER IF EXISTS audit_events_snaplink_delivery ON audit_events`、:130-133 `CREATE TRIGGER … AFTER INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION aero_enqueue_snaplink_audit()`（函数本体 :68-127）。先例要点：runtime-enabled 门（关 ⇒ `RETURN NEW` 零 outbox 行）、binding 缺失 RAISE 中止整事务（fail-closed）、payload `action = NEW.action` **原样透传**（今日无映射）。0239 触发器须并行共存、不破坏此门（E9/A2 halves 钉）。`audit_events` 本表 DDL = `migrations/0007_audit.sql:8-22`（id/workspace_id/actor_id/action/target/detail/created_at——触发器可读全列）。 |
| E7 | （补充）relay boot 降级语义 | ✅ **Verified**。`crates/aero-server/src/bin/main.rs:245-266` B5-2 段：presence-gated（`RelayConfig::from_env`），注释原文 **"booting before the 0239 table lands degrades to logged claim errors, not a crash"**（F13 既定降级，非缺口）；spawn `relay.spawn(ai_shutdown.clone())`。 |
| E8 | （补充）`aero-audit-connector` 状态机与测试套件 | ✅ **Verified**。`relay.rs`：`dispatch_batch` :156 / `deliver_claim` :173-231（Ok→settle；transient→requeue 永不 dead；permanent（422/409/回执/payload-guard）→attempt 1 requeue、≥2 mark_dead；**403→首次即 mark_dead（T-11）**）；`audit_backoff`/`clamped_lease`/`truncate_error` :56-78。测试：`src/relay.rs` 6 项（transient 3 + backoff/dead-threshold/lease-clamp）+ `tests/state_machine.rs` 6 项 + `tests/claim_validation.rs` 10 项 + `config.rs` 2 项 = **24 项非 ignored 全绿**（B5-2 spec 实跑记录）+ `pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（`--ignored`，自带 0239 等效表探测+自建）。direction 验收的「37-tests」是**仓外契约清单**的标签（B5-2 spec §7：「37/37 清单在仓外」），in-repo 规范化读法 = 上述套件名 + 本方向 drill（§5 A4）。 |
| E9 | （补充）integration 门控锚点 | ✅ **Verified**。`scripts/test-integration.sh`：:306-318 `audit_governance::` + `moderation_finalize_outbox_parity` 两个 `run_migrated_integration` 条目（空过滤守卫防 vacuous green）；:325-340 A3 relay drill 段；:347-397 T-11 drill 段（含 B5-4 provision leg D/C，直接 `UPDATE audit_governance_outbox SET status = 3 …` 与 `outbox-0239: table=audit_governance_outbox …` 断言）；:441-457 moderation-priority drill 段（同文件门控，FAIL 红语义见 E3）。全部 `else` 分支 = 显式 `b5_check … SKIP (0239 not landed)`。 |

### 1.1 对 direction problem 陈述的勘误/钉化（evidence-backed）

- **「0239 列默认 priority 10」出处确认**：`governance.rs:33` doc（"also the 0239 column default"）——**不是** :32（:32 是 MODERATION=100 的 doc 行）。两值都进 DDL：默认 10（backlog 车道），moderation 行由触发器显式写 100。
- **priority drill 的车道常量（BACKLOG=100 / MODERATION=200）≠ governance.rs（10 / 100）**：drill 自身注释标 [PROPOSED]（"if B5-3 pins different lane values, adjust these two constants only"）。0239 只钉**列默认 10**（drill 种子行显式带 priority，不依赖默认）；100/200 vs 10/100 的调和属 B5-3 方向，**不是本 direction 的验收**（§7 风险）。
- **触发器形态钉为「并行新增」，非「重定向改写」**：direction 措辞 "extend/parallel with the enqueue redirect"。验收只要求 moderation → v2 行同事务、非 moderation pass-through 不 raise；**不改写/不删除 0236 的 `audit_events_snaplink_delivery` 触发器**即满足（并行 AFTER INSERT 触发器，各自独立；0236 binding RAISE 仍是唯一 abort 路径）。v1/v2 共存与「moderation 行是否也进 v1」是 campaign 决策（B5-1 设计文档 §5.5），出范围。
- **「同事务」的机制确认**：AFTER INSERT **行级**触发器在插入事务内执行 ⇒ audit_events 行 + outbox 行与软删同提交/同回滚，无需任何 Rust 侧改动（`handle_moderate` 调用形状冻结，E5）。

## 2. Verified current state

```
producer（已在，同事务，零改动）          crates/aero-ai/src/worker/mod.rs:372-404（LOCAL_ACTION_MODERATED @401）
                                          crates/aero-im-core/src/service/messages.rs:620-644（:636 workspace.map）
                                          crates/aero-storage/src/message/events.rs:267,337（soft_delete_outboxed_system
                                          → AuditRepo::append_in_tx 同 tx）
                                          crates/aero-storage/src/message/crud.rs:398 · message_reports.rs:305（同 token）

mapping 权威（已在，untracked ⚠️）        crates/aero-ai/src/governance.rs（class/priority/status 契约 + 6 单测）

0236 先例（已在）                        migrations/0236_snaplink_governance_reconciliation.sql:68,129-133
                                          （AFTER INSERT 触发器 + runtime 门 + binding RAISE）

connector 消费方（已在，untracked）       crates/aero-audit-connector/（pg.rs SQL 指名列 + 3 drill + 24 单测 + 1 PG 测试）

boot 降级（既定）                        crates/aero-server/src/bin/main.rs:245-266（F13：logged claim errors 不 crash）

integration 门（已在）                   scripts/test-integration.sh:306-457 + scripts/b5-pin.sh:37-38
                                          （全部 `[ -f migrations/0239_audit_governance_outbox.sql ]` 门控）

缺的唯一一环 = 0239 迁移文件（migrations/ 尾号 0238，E1）
```

**未闭合项**（本 direction 关闭）：0239 表不存在 → 3 个 drill exit 2 SKIP、`audit_governance::`/`moderation_finalize_outbox_parity`/`a3-relay-drill`/`t11-fail-closed`/`moderation-priority-drill` 全部 SKIP、audit 行永不到 sink。relay 侧（B5-2）与映射侧（governance.rs）**已完整就位**——本 direction 是唯一阻塞。

## 3. Scope

**In scope（B5-1 aero-ai slice）**：
- 迁移文件 `migrations/0239_audit_governance_outbox.sql`：`audit_governance_outbox` 表 DDL（status 0/1/2/3 规范值、class/priority 列 + 默认、claim 列全、due 索引）。
- `audit_events` AFTER INSERT 触发器（并行新增）：`message.moderated` → 同事务 v2 行（class='admin'、priority=100、status 0、event_id=NEW.id 1:1）；未知 token pass-through 不 raise（R1 钉）。
- 验收 oracles 全量跑绿：t11 / relay / priority 三 drill + `audit_governance::` 与 `moderation_finalize_outbox_parity` integration 条目 + 既有 connector 单测无回归。
- 提交 `crates/aero-ai/src/governance.rs`（当前 untracked，映射权威必须随本 direction 落库）。

**Out of scope**：
- `AuditGovernanceOutboxRepo` 仓储 + aero-storage db_tests（`audit_governance::` 族）→ sibling direction `land-b5-1-migration-0239-audit-governance-outbox-3bfbc22b`（crates/aero-storage）；本 direction 只定义 DDL 契约供其消费，**迁移文件只落一次**，两方向验收共用同一文件。
- `claim_due` 的 `ORDER BY priority DESC` + 反饥饿 → **B5-3**（priority drill 的 PASS 归 B5-3；本 direction 只保证其列探测不 SKIP）。
- 配给门（`audit-provision-check`）→ **B5-4**（其 0239 分布/status=3 断言依赖本 direction 的表）。
- v1/v2 共存与 0236 重定向改写、L1 聚合（[PROPOSED]）→ campaign 决策。
- `AiWorker`/`messages.rs`/connector 任何代码改动：**禁止**（producer 调用形状冻结，E5/E7）。

## 4. Requirements

### R1 — 0239 迁移：`audit_governance_outbox` DDL（消费者契约全集）
文件 `migrations/0239_audit_governance_outbox.sql`（文件名被 `scripts/test-integration.sh` 四处门控钉死，E1）。`CREATE TABLE IF NOT EXISTS audit_governance_outbox`，列 = 消费方（E3/E4）逐列点名的并集：

| 列 | 类型 | 约束 | 消费者钉点 |
|---|---|---|---|
| `event_id` | uuid | PRIMARY KEY（= `audit_events.id`，1:1 幂等键 + 出站 Idempotency-Key） | pg.rs claim/settle/requeue/mark_dead 全部 `WHERE event_id = $1`；relay-drill parity |
| `payload` | jsonb | NOT NULL | pg.rs RETURNING payload → 出站 POST 体 |
| `status` | integer | NOT NULL DEFAULT **0**；CHECK `IN (0,1,2,3)` | pg.rs:26-30 规范值；t11/relay/priority drill 断言 0/2/3 |
| `class` | text | NOT NULL DEFAULT **'message'**（admin/message/room，governance.rs:44-50） | t11/relay drill INSERT 不含此列 ⇒ 默认必需；priority drill 写 'admin'/'message' + 列探测 |
| `priority` | integer | NOT NULL DEFAULT **10**（= `GOVERNANCE_PRIORITY_BACKLOG`，governance.rs:33「0239 column default」） | 同上：默认必需（t11/relay drill 不写此列）；moderation 行显式 100 |
| `available_at` | timestamptz | NOT NULL DEFAULT `clock_timestamp()` | pg.rs claim 过滤 + backoff re-park（单时钟域） |
| `created_at` | timestamptz | NOT NULL DEFAULT `clock_timestamp()` | pg.rs claim ORDER BY 第三键 |
| `attempts` | bigint | NOT NULL DEFAULT 0 | t11 drill `SUM(attempts)` 逐轮 N/2N；requeue/mark_dead fence |
| `claim_token` | uuid | NULL | 轮换 fencing token |
| `lease_expires_at` | timestamptz | NULL | claim 过滤 + settle/requeue/mark_dead 栅栏 |
| `delivered_at` | timestamptz | NULL | settle 置 `clock_timestamp()`；priority drill 排序断言 |
| `last_error` | text | NULL | t11 drill `LIKE '%audit connector HTTP transport failed%'` |

due 索引（镜像 0235:187 先例，claim 过滤面）：`CREATE INDEX … ON audit_governance_outbox (available_at, created_at, event_id) WHERE status IN (0, 1)`。迁移幂等（`IF NOT EXISTS` + `DROP TRIGGER IF EXISTS` 先例，0236:129）。**落地顺序钉死（AGENTS.md §4.2）：先 `cargo build` 再 `aero-cli migrate`**——迁移编译期嵌入 bin，否则静默 no-op。

### R2 — `audit_events` AFTER INSERT 触发器：同事务 enqueue（并行新增，不改 0236）
`CREATE FUNCTION aero_enqueue_governance_audit()` + `CREATE TRIGGER audit_events_governance_enqueue AFTER INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION …`（复制 0236:129-133 形态；**不得** DROP/改写 `audit_events_snaplink_delivery`）。行为：
- `NEW.action = 'message.moderated'` → `INSERT INTO audit_governance_outbox (event_id, payload, class, priority, status) VALUES (NEW.id, <envelope>, 'admin', 100, 0)`——status 0 显式写（= `GOVERNANCE_PRIORITY_MODERATION` 100 / `GOVERNANCE_CLASS_ADMIN`，governance.rs:31,45）。
- 其余 action → `RETURN NEW`，零行、**零 RAISE**（R1 pass-through 钉：`governance_lane_for` 的 `None` 分支是唯一映射面；0236 binding RAISE 仍是唯一 abort 路径）。
- 映射**仅键于 action token**，不读调用者身份（token-keyed，E5 三 producer 同形）。
- payload envelope 镜像 0236 形状 + drill fixture 键（E3 种子 `{"event_id": …, "source_system": "aero-im.source"}`）：至少含 `event_id`（= NEW.id::text）、`source_system`（'aero-im.source'，与 main.rs/relay config 一致）、`action`（NEW.action 原样本地 token）、`workspace_id`、`actor_id`、`target`、`detail`、`created_at`——经 sanitize 先例（0236 `aero_sanitize_snaplink_audit_value`）后入 payload。
- 触发器内 class/priority 字面量旁加 **cross-slice pin 注释**（`// cross-slice pin: must equal aero_ai::governance::{GOVERNANCE_CLASS_ADMIN, GOVERNANCE_PRIORITY_MODERATION, …}`——aero-storage 生产代码不得 import aero-ai，B5-1 设计文档 §5.1 先例）。
- **gate 语义保持**（0236 先例 + sibling A2 halves `moderation_finalize_without_binding_aborts_tx` / `moderation_finalize_runtime_disabled_commits_1_plus_0` 钉）：runtime disabled ⇒ 零 outbox 行不 raise；binding 缺失 ⇒ 0236 RAISE 中止整事务（本触发器随之回滚，无独立 abort 路径）。

### R3 — 原子性（同事务三件套）
moderation finalize 的单个 PG 事务 = 软删 UPDATE（events.rs:319）+ `AuditRepo::append_in_tx`（:337，audit 行含本地 token 'message.moderated'）+ **本触发器产出的 outbox 行** + `RoomEvent::Deleted` event outbox；任一失败（含 0236 binding RAISE）→ 全回滚。无 post-commit best-effort enqueue、无异步步骤。`handle_moderate` 的 `Err` 传播 → 重试 → DLQ 语义不变（worker/mod.rs:422）。

### R4 — 消费方零改动回归
relay（claim→settle/requeue/mark_dead）、pg.rs SQL、三 drill 二进制、`main.rs` 接线、governance.rs 一律**不改**。本 direction 的交付 = 迁移文件（+ 其测试门控解锁）+ 提交 untracked 的 governance.rs。`git diff` 除新增迁移外应为空。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> 前置（每个 throwaway 库验证）：`cargo build`（迁移编译期嵌入）→ 全新一次性库 `CREATE DATABASE` → `aero-cli migrate` 全链 replay（AGENTS.md §4.3/4.2）。

### A1 — `aero-audit-t11-drill` 与 `aero-audit-relay-drill` 从 SKIP（exit 2）翻转为 PASS
- **Oracle A**（relay）：throwaway 库迁移后 `DATABASE_URL=… cargo run -p aero-audit-connector --bin aero-audit-relay-drill` → **exit 0**，stdout 末行 `PASS: {N}/{N} delivered (status 2), event_id set-parity exact, stub POSTs {N}`（N = `AERO_AUDIT_DRILL_ROWS` 默 3）。原先 :57-67 的 `to_regclass` NULL 分支不再命中（表存在）。
- **Oracle B**（t11）：`… --bin aero-audit-t11-drill` → **exit 0**，末行 `drill: t11-pending: PASS`；两轮断言逐项成立：`COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、`SUM(attempts)` = N 后 2N（round 2 前 sleep 1.2s > backoff(1)=1s，重试-永不放弃姿态）、`last_error LIKE '%audit connector HTTP transport failed%'` 全行（封闭 token 端点确实被尝试）。
- **Integration 面**：`bash scripts/test-integration.sh` 中 `a3-relay-drill` 与 `t11-fail-closed` 两个 `b5_check` 条目从 `SKIP (0239 not landed)` 翻为 `PASS`（:325-397 段执行，不再走 :340/:395 的 else 分支）。
- **反向断言**：`to_regclass('audit_governance_outbox')` 非 NULL；`ls migrations/0239_audit_governance_outbox.sql` 存在。

### A2 — T-11：moderation finalize（`message.moderated`）同事务提交 audit_events 行 + outbox 行（status 0、event_id = audit_events.id、class='admin'、priority=100）；软删回滚则两行俱回滚
- **Oracle**：`scripts/test-integration.sh:309-314` 的 `moderation_finalize_outbox_parity` 条目（sibling storage slice 的 db_test；`run_migrated_integration` **空过滤守卫**要求该测试名真实存在且 ≥1 测试匹配——A1.3 防 vacuous green；未落测试 = 条目 FAIL）。
- 断言（对 BLOCK verdict 走 `AiWorker::handle_moderate` 生产 seam，非 bespoke 路径）：
  1. 恰 1 条 `audit_events` 行：`action = 'message.moderated'`（本地 token 原样，R2 envelope 也带）；
  2. 恰 1 条 `audit_governance_outbox` 行：`status = 0`、`event_id = audit_events.id`（1:1 parity）、`class = 'admin'`、`priority = 100`、payload 含 `event_id`/`source_system`/`action`；
  3. 同事务证明：先 `BEGIN` 执行 moderation finalize 到 audit append 后**强制回滚**（如 0236 binding 缺失 RAISE 的 `moderation_finalize_without_binding_aborts_tx` 场景，或事务级回滚注入）→ 软删未生效 **且** audit 行 **且** outbox 行三者俱不存在（无「deleted but unqueued / audited but unqueued」半态）；
  4. runtime disabled（0236 门）→ 1 audit 行 + **0** outbox 行（`moderation_finalize_runtime_disabled_commits_1_plus_0` 语义保持）。
- 计数断言对齐 B5-1 常量（`STATUS_ENQUEUED=0`，pg.rs:26）而非硬编码数字（B5-2 spec §7 状态枚举归属钉）。

### A3 — 0239 触发器对非 moderation audit 行 pass-through（governance.rs R1 钉；无第二 RAISE abort 路径）
- **单元面**（已在，回归即验收）：`cargo test -p aero-ai governance::` → `unknown_local_token_passes_through_unmapped`（`room.create`/`message.create`/`message.edit`/`message.deleted`/`call.join`/`""` 全部 `None` + 非 admin-class）+ `user_delete_token_stays_out_of_admin_lane`（R-D2 负钉：`message.deleted` 绝不进 admin 车道）+ `mapping_is_token_keyed`。
- **PG 面**：非 moderation audit 行（如 `action='room.create'`）在事务内提交 → 0 条 outbox 行、事务**不**因本触发器中止（无第二 RAISE）；`audit_governance::` db_test 族中非 moderation 家族断言同样成立（sibling slice 的 A2 非 moderation 半）。
- **共存面**：0239 落库后 `audit_events_snaplink_delivery` 触发器仍存在且行为不变（runtime 门 + binding RAISE；`aero_enqueue_snaplink_audit` 函数未被改写）——0236 的既有测试/门控语义无回归。

### A4 — relay 状态机：claim → status 1 → settle → status 2 + delivered_at；requeue 退避；mark_dead → status 3（37-tests 无回归）
- **「37-tests」in-repo 规范化读法**（仓外契约清单标签，E8）：`cargo test -p aero-audit-connector` → **24/24 绿**（relay.rs 6：`transient_{5xx,timeout,claim_drift}_requeues_and_rotates_a_fresh_token` / `backoff_is_bounded_and_exponential` / `permanent_dead_threshold_is_pure_and_total` / `lease_is_clamped_into_the_proven_bounds`；`tests/state_machine.rs` 6：`forbidden_dead_on_first_attempt` / `permanent_error_dead_after_exactly_two_attempts` / `stale_token_cannot_ack_after_reclaim` / `happy_path_settles_and_removes_from_claimable` 等；`tests/claim_validation.rs` 10；`config.rs` 2）+ `pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（`--ignored`，`DATABASE_URL` 门控，实跑）——**状态机/claim/fence 语义零回归**（本 direction 不改 connector 一行，R4）。
- **状态迁移在真表上的端到端 oracle**（0239 落库前不可达，落库后即验证）：
  - claim → status 1：t11 drill 两轮 `SUM(attempts)` 证据 + pg.rs 并发双 claim 测试（50 行两 session 各 25、零交集、attempts 恒 1）；
  - settle → status 2 + `delivered_at`：relay drill `COUNT(status=2)==N` + parity；priority drill round 1 的 `MIN(delivered_at)` 断言；
  - requeue 退避：t11 drill round 间 1.2s sleep 后行重新可 claim（`available_at` 按 backoff re-park）；
  - mark_dead → status 3：`tests/state_machine.rs::forbidden_dead_on_first_attempt`（403 → attempts==1 即 Dead、posts==1、后续 claim 空）+ t11 drill `COUNT(status IN (1,2,3))==0` 反向断言（无假死）。
- **priority drill 的边界语义**（E3 钉死，诚实信号）：0239 落库后 `aero-audit-priority-drill` **必须不再 exit 2 SKIP**（表 + priority/class 列探测通过）；其 PASS 依赖 B5-3 排序——B5-3 未落时 drill FAIL 红属**预期**，不是本 direction 的回归（§7）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| 表/列存在 + drill 翻转（A1） | `crates/aero-audit-connector/src/bin/aero-audit-relay-drill.rs`（:57-67 probe）/ `aero-audit-t11-drill.rs`（:73-83 probe）/ `aero-audit-priority-drill.rs`（:87-118 probe） | integration：throwaway 库 + 全链迁移 + stub sink；`scripts/test-integration.sh:306-457`（0239 文件门控解锁） |
| 同事务原子性 + 列值（A2） | sibling slice 的 aero-storage db_test：`moderation_finalize_outbox_parity` + `audit_governance::` 族（含 `moderation_finalize_without_binding_aborts_tx` / `moderation_finalize_runtime_disabled_commits_1_plus_0`，B5-1 设计文档 §6 A2 命名钉） | `run_migrated_integration`（`--test-threads=1`，空过滤守卫），`DATABASE_URL` + 已迁移 |
| pass-through 单元（A3） | `crates/aero-ai/src/governance.rs` tests（已在：`unknown_local_token_passes_through_unmapped` / `user_delete_token_stays_out_of_admin_lane` / `mapping_is_token_keyed` / `outbound_action_is_single_contract_token` / `admin_class_rows_never_aggregated` / `moderation_lane_preempts_backlog_under_desc_claim`） | `cargo test -p aero-ai governance::`，无 DB |
| 状态机无回归（A4） | `crates/aero-audit-connector/tests/state_machine.rs`（6）+ `tests/claim_validation.rs`（10）+ `src/relay.rs`（6）+ `src/config.rs`（2）+ `src/pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（1，`--ignored`） | `cargo test -p aero-audit-connector`（+ `-- --ignored` 带 `DATABASE_URL`） |
| 提交门禁 | governance.rs 从 untracked 转 committed；`git diff` 除新迁移外为空（R4） | review + `cargo clippy --workspace --all-targets` 无新警告 + `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 |

## 7. Risks / [PROPOSED]

- **`crates/aero-ai/src/governance.rs` 当前 untracked**（上一批次产物）：映射权威（R1 列默认/class/priority 契约来源）必须随本 direction 提交，否则 DDL 字面量无 in-repo 锚点。提交前跑其 6 项单测。
- **priority drill 车道常量漂移（100/200 vs 10/100）**：drill 注释自标 [PROPOSED]；B5-3 调和。本 direction 只钉列默认 10；若 B5-3 改默认，改的是 0239 DDL 默认值 + drill 常量二处（cross-slice pin 注释指路）。
- **priority drill 在 B5-3 未落时的 FAIL 红**：是设计（G6 诚实信号），不是回归；`scripts/test-integration.sh:441-457` 段在该窗口会红——campaign 排期上 priority drill 的 PASS 归属 B5-3 落地后。
- **sibling 方向重叠**（`land-b5-1-migration-0239-audit-governance-outbox-3bfbc22b`，crates/aero-storage）：迁移文件只落一次；本 spec §4 R1/R2 即共享 DDL 契约（列/默认/触发器行为），仓储 db_test 归 sibling，两方向验收互相解锁（A2 测试名归 sibling、A1/A3 归本方向）。
- **v1/v2 共存与 0236 重定向**：campaign 决策，出范围；本 direction 的并行触发器形态保证 0236 行为零变化（A3 共存面）。
- **「37-tests」为仓外契约标签**：本 spec 以 in-repo 套件名 + 计数（24 非 ignored + 1 ignored PG + 3 drill）为可执行读法；契约原文若冲突，改的是标签映射，不是测试。
- **迁移编译期嵌入**：漏 `cargo build` 直接 `aero-cli migrate` → 0239 静默 no-op、drill 仍 SKIP——A1 的「反向断言」（文件存在 + to_regclass 非 NULL）防此坑。

## 8. Sequencing

1. **落 0239**：写 `migrations/0239_audit_governance_outbox.sql`（R1 DDL + R2 触发器）→ `cargo build` → throwaway 库 `aero-cli migrate` → 手工跑三 drill（A1/A4 oracle 先行，快速反馈）。
2. **提交映射权威**：`crates/aero-ai/src/governance.rs` 入库 + 其单测绿（A3 单元面）。
3. **integration 全链**：`bash scripts/test-integration.sh`——`a3-relay-drill` / `t11-fail-closed` 翻 PASS；`audit_governance::` 与 `moderation_finalize_outbox_parity` 由 sibling slice 的 db_test 满足（空过滤守卫强制真实匹配，否则 FAIL）。
4. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规；`git diff` 除新迁移外为空（R4 no-touch）。
5. **B5-3（priority DESC）/ B5-4（provision）** 随后并行：priority drill 翻 PASS 归 B5-3；`audit-provision-check` 的 0239 分布断言（test-integration.sh:400-437 leg D/C）依赖本 direction 的表已就位。
