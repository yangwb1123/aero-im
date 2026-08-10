# Requirements Spec — B5-1: 落库 0239 `audit_governance_outbox`（status 0/1/2/3 + class/priority + 同事务 enqueue-redirect 触发器）

- **Module (analysis root)**: `crates/aero-ai/src`（governance.rs = 映射权威 + 验收 oracle；DDL 落在 `migrations/`，与 sibling aero-storage slice 共享）
- **Direction**: "Land migration 0239: audit_governance_outbox DDL (status 0/1/2/3 + class/priority) with the in-tx enqueue-redirect trigger"（value 9 / risk_reduction 9 / effort 7 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json` (direction #1)
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（v2 契约正文在仓外，[PROPOSED]）；门禁 G6 = "37/37、T-11、moderation 优先级"
- **Sibling specs（同契约不同切片）**: `docs/requirements/2026-08-07-aero-ai-b5-1-migration-0239-governance-outbox.req.md`（同一 direction 标题，但基于另一份 analysis `crates-aero-ai-f8cd3622.json`，其 E1「0239 从未存在」已被本工作树状态取代）、`docs/requirements/2026-08-06-aero-ai-b5-1-audit-governance-outbox.req.md`（本 analysis 的旧 direction #1 表述）
- **Status**: Requirements（下述证据全部经源码 grep + 实跑核对；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

> ⚠️ **关键状态校正（本 spec 与 analysis 的最大差异）**：analysis 生成时（2026-08-07 14:21）`migrations/` 尾号为 0238、0239 不存在；**核对时（2026-08-08）工作树已含完整落地**——`migrations/0239_audit_governance_outbox.sql` + `migrations/0240_audit_governance_due_prio_idx.sql`（均为 untracked）、`crates/aero-audit-connector/`、`crates/aero-ai/src/governance.rs`、`crates/aero-eng/src/audit_provision.rs`、`crates/aero-storage/src/audit_governance.rs`、drills、`scripts/b5-pin.sh` 全数存在。本 direction 描述的空缺已被工作树闭合；**本 spec 的职责 = 把 DDL/触发器契约钉死为可验证需求 + 验收 oracles 全部可执行**，并把「未提交」列为必须关闭的交付项。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `migrations/`（"last numbered: 0238_message_recall.sql — no 0239"） | ⚠️ **已过时（核心校正）**。`ls migrations/*.sql \| wc -l` = **240**；tracked 尾号确为 `0238_message_recall.sql`，但工作树 untracked 已有 `0239_audit_governance_outbox.sql` 与 `0240_audit_governance_due_prio_idx.sql`（`git status --short migrations/` 双双 `??`）。文件名被 in-repo 消费者钉死：`scripts/test-integration.sh` 多处 `[ -f "migrations/0239_audit_governance_outbox.sql" ]` 门控（:305-322 B5-1 条目、:325+ A3 relay、:347+ T-11、:441+ priority），`scripts/b5-pin.sh:37-44` 命名槽 `audit_governance::` / `moderation_finalize_outbox_parity` / `a3-relay-drill` / `t11-fail-closed` / `moderation-priority-drill`。**0240 属 B5-3（priority DESC 索引），非本 direction 交付**（0239 文件头注释自述："B5-3 extends ORDER BY with priority; the index change is B5-3's, not this slice's"）。 |
| E2 | `crates/aero-audit-connector/src/pg.rs`（STATUS 0/1/2/3；claim_due/settle/requeue/mark_dead SQL；ensure_outbox_table 最小回退） | ✅ **Verified**。`STATUS_ENQUEUED=0/CLAIMED=1/DELIVERED=2/DEAD=3` 常量 :28-31（doc："**B5-1 0239 DDL normative values**"）。`claim_due` :69-108：`FROM audit_governance_outbox`、`status IN (0,1)`、`available_at <= clock_timestamp()`、`(lease_expires_at IS NULL OR <= clock_timestamp())`、`ORDER BY (available_at, created_at, event_id)`（**无 priority 项**——B5-3 的事）、`FOR UPDATE SKIP LOCKED`、`LIMIT` clamp `[1,500]`（`MAX_CLAIM`）。`settle` :110-153（单事务 fenced 重读 → status=2 + delivered_at）；`requeue` :155-187（status=0 + backoff re-park）；`mark_dead` :189-217（status=3）。单时钟域：全用 `clock_timestamp()`，trait 无 caller `now`（防 app/DB 时钟偏移 livelock）。测试 `ensure_outbox_table` :244-270：`to_regclass` 探测真 0239，缺失则自建**最小等效形（无 class/priority）**——反向证明真实 0239 必须含这两列。 |
| E3 | `migrations/0236_snaplink_governance_reconciliation.sql`（v1 AFTER INSERT 触发器） | ✅ **Verified**。`aero_enqueue_snaplink_audit()` :68（runtime-enabled 门：关 ⇒ `RETURN NEW` 零行；`aero_snaplink_binding_for_workspace` 缺失 RAISE 中止整事务）；触发器 `audit_events_snaplink_delivery` :129-133（`AFTER INSERT ON audit_events FOR EACH ROW`）。v1 表 `snaplink_delivery_outbox`（0235:161）无 status/class/priority。0239 触发器与它**并行共存**（'g' < 's' 触发序，0239 注释自述）；0236 的 binding RAISE 仍是唯一 abort 路径。 |
| E4 | `crates/aero-eng/src/audit_provision.rs`（G0239_CANDIDATES；Q3 四桶查询） | ✅ **Verified**。`G0239_CANDIDATES: [&str; 2] = ["audit_governance_outbox", "audit_outbox"]` :26（漂移单点）；探测 SQL :43（两候选 `to_regclass`）；`Q3_SQL` :48 = `SELECT status, count(*) FROM {table} GROUP BY status ORDER BY status`（四桶 0/1/2/3，缺桶补 0，`parse_buckets` :222 未知 status 忽略——0239 CHECK 保证 0..3）；`outbox-0239: table=… pending=… claimed=… delivered=… dead=…` 输出行 :148；verdict :109-112 **dead 优先 fail-closed**——"dead is never counted delivered"。单测 `verdict_dead_is_fail_closed_even_with_relay_on` :106 / `verdict_dead_priority_over_relay_disabled` :136 钉此语义。 |
| E5 | 三 drill 表探测（t11-drill.rs:73 / relay-drill.rs:57 / priority-drill.rs:87） | ✅ **Verified**。t11-drill:73-83 与 relay-drill:57-67：`SELECT to_regclass('audit_governance_outbox')::text` 为 NULL → `eprintln!(SKIP …)` + `std::process::exit(2)`。priority-drill:87-97 同款表探测 + :105-118 `information_schema.columns` 探测 `priority`/`class` 两列（缺列同样 exit 2 SKIP）。**关键语义**（priority-drill:28-33 doc）：0239 落库但 B5-3 排序未落 → **FAIL 红而非 SKIP**（诚实 G6 信号）。drill 断言即 oracle：t11 = 两轮 `COUNT(status=0)==N` / `COUNT(status IN (1,2,3))==0` / `SUM(attempts)` N→2N（round 间 sleep 1.2s > backoff(1)=1s）/ `last_error LIKE '%audit connector HTTP transport failed%'` 全行，末行 `drill: t11-pending: PASS`（:203）；relay = `PASS: {N}/{N} delivered (status 2), event_id set-parity exact, stub POSTs {N}`（:164）。**t11/relay drill 的种子 INSERT 不含 class/priority 列** ⇒ 两列默认值（'message'/10）必需。 |
| E6 | `docs/proposals/audit-contract-batch-aero-im.md`（B5-1） | ✅ **Verified（as proposed）**。in-repo 文件是 15 行门摘要（294 行全文在仓外）：B5-1 = 新迁移 `0239_audit_governance_outbox.sql`——status 0/1/2/3 normative、`class` message/room/admin、`priority`、`delivery_mode`、`CREATE OR REPLACE` 重定向 enqueue/reconcile 函数、P2 parity = `event_id` 1:1 断言。落地实现（E8）与此形状一致，但**采用「并行新增触发器」而非改写 0236**（见 §1.1 勘误）。 |
| E7 | （补充）`crates/aero-ai/src/governance.rs`——映射权威 | ✅ **Verified（实跑 6/6 绿）**。`GOVERNANCE_PRIORITY_MODERATION: i16 = 100` :31 / `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` :33（doc 明言 **"also the 0239 column default"**）；`GOVERNANCE_CLASS_ADMIN/MESSAGE/ROOM` :37-41；`LOCAL_ACTION_MODERATED = "message.moderated"` :49；`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` :56（双候选锁定为一）；`GovernanceLane` :61-72（class/priority/outbound_action/status 四字段）；`governance_lane_for` :86-99（`message.moderated` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`；未知 token → `None` pass-through，永不 raise/block）；`is_admin_class` :102-106（L1 bypass 分类权威）。6 项单测：`moderation_lane_preempts_backlog_under_desc_claim` / `unknown_local_token_passes_through_unmapped`（含 `message.deleted`、`""`）/ `user_delete_token_stays_out_of_admin_lane`（R-D2 负钉）/ `mapping_is_token_keyed` / `outbound_action_is_single_contract_token` / `admin_class_rows_never_aggregated`。**实跑**：`cargo test -p aero-ai governance::` → `6 passed; 0 failed`。⚠️ 文件 **untracked**——必须随本 direction 提交（§7）。 |
| E8 | （补充）已落地的 `migrations/0239_audit_governance_outbox.sql` 内容 | ✅ **Verified（本 direction 的交付物现状）**。`CREATE TABLE IF NOT EXISTS audit_governance_outbox`：`event_id uuid PRIMARY KEY`（= audit_events.id，1:1 + 出站 Idempotency-Key）、`status integer NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))`、`class text NOT NULL DEFAULT 'message'`、`priority smallint NOT NULL DEFAULT 10`、`delivery_mode text NOT NULL DEFAULT 'push'`（保留，无消费者）、`payload jsonb NOT NULL CHECK (jsonb_typeof(payload)='object')`、`available_at/created_at timestamptz DEFAULT clock_timestamp()`、`attempts bigint DEFAULT 0`、`claim_token uuid`、`lease_expires_at`、`delivered_at`、`last_error`、claim-state CHECK（token 与 lease 同 NULL 或同非 NULL，镜像 0235）。due 索引 `audit_governance_due_idx (available_at, created_at, event_id) WHERE status IN (0,1)`。函数 `aero_enqueue_governance_audit()`：runtime 门（disabled ⇒ RETURN NEW 零行，镜像 0236:75-79）→ token 门（`action <> 'message.moderated'` ⇒ RETURN NEW pass-through，零 RAISE）→ binding 查找（缺失 RAISE 中止整事务，fail-closed，独立于 0236 触发序）→ `INSERT … (event_id, status, class, priority, payload) VALUES (NEW.id, 0, 'admin', 100, jsonb_build_object(…))`（envelope 含 event_id/source_system='aero.im.security'/event_type/schema_id/schema_version/occurred_at/actor/targets/aggregate_type/aggregate_id/action='admin.content.flag'/outcome/payload/data_classification/retention_class/idempotency_key）+ `ON CONFLICT (event_id) DO NOTHING`（幂等）。触发器 `audit_events_governance_enqueue AFTER INSERT ON audit_events FOR EACH ROW`。**cross-slice pin 注释**齐备（列默认 ↔ governance.rs:33/:34；moderation 戳 ↔ governance.rs:31/:33/:60，键 LOCAL_ACTION_MODERATED :49）。 |
| E9 | （补充）integration 门控锚点 | ✅ **Verified**。`scripts/test-integration.sh`：:147 `source …/b5-pin.sh`（direction 引的 :143 即此段）+ :149 guard 自测；:305-322 `audit_governance::` 与 `moderation_finalize_outbox_parity` 两条 `run_migrated_integration`（**空过滤守卫**：测试名必须真实匹配 ≥1 测试，防 vacuous green；else 分支 `b5_check … SKIP (0239 not landed)`）；:325-340 A3 relay drill 段；:347-397 T-11 drill 段（含 B5-4 provision leg D/C：直接 `UPDATE audit_governance_outbox SET status = 3` 与 `outbox-0239: table=audit_governance_outbox …` 断言）；:441-457 moderation-priority drill 段（FAIL 红语义见 E5）；:243-298 audit-provision-check leg B1/B2。`scripts/b5-pin.sh`：恰 37 槽（15 执行 + 22 `[PROPOSED]`），`assert_b5_contract_pin` 守卫（恰 37/无重复/无 malformed/非空转/verdict 行 `B5-CHECK <name>: PASS|SKIP`）；`scripts/test-b5-pin-guard.sh` 存在。 |
| E10 | （补充）relay boot 降级 + 403/422 dead 语义 | ✅ **Verified**。`crates/aero-server/src/bin/main.rs:245-261` B5-2 段：presence-gated（`RelayConfig::from_env`），注释原文 "booting before the 0239 table lands **degrades to logged claim errors, not a crash**"（F13 既定降级）；spawn `relay.spawn(ai_shutdown.clone())`。`crates/aero-audit-connector/src/relay.rs:15-17` doc + `deliver_claim` :173-226：**403 → 首次即 `mark_dead`**（:190-204，T-11 fail-closed，无 requeue）；**permanent（422/409/回执错/payload guard）→ attempt 1 requeue、attempt ≥2 `mark_dead`**（:208-220，`is_dead_at(attempts) = attempts >= PERMANENT_DEAD_AT` :48-49，"dead after ≤1 retry"）。`tests/state_machine.rs`：`forbidden_dead_on_first_attempt` :231-247（403 → 首 attempt 即 Dead，posts==1）与 `permanent_error_dead_after_exactly_two_attempts` :145（422 → 两 attempt 后 Dead）——**DB-free（FakeOutbox），非 ignored**。 |
| E11 | （补充）sibling storage slice db_tests | ✅ **Verified**。`crates/aero-storage/src/audit_governance.rs`：`moderation_finalize_outbox_parity` :211（= b5-pin 槽名）、`ddl_contract_defaults_and_checks` :382、`moderation_finalize_runtime_disabled_commits_1_plus_0` :482、`non_moderation_action_passes_through_unmapped` :527、`duplicate_event_id_is_deduped_by_on_conflict` :586——全部 `#[ignore = "requires live Postgres"]`，经 `run_migrated_integration` 在 throwaway 已迁移库跑。 |

### 1.1 对 direction problem 陈述的勘误/钉化（evidence-backed）

- **「表不存在」已闭合**（E1）：工作树已有 0239（+0240 sibling）。本 direction 的剩余交付 = **提交 + 全链验证**，spec §4/§5 即钉死的契约与 oracles。
- **「same-tx audit write for message.*/room.*/admin.* 不完整」的准确边界**：0239 触发器只映射 `message.moderated`（唯一已钉 token，E7）；message.create/edit/room.* 等**没有** audit 行产出是 B5-1 storage slice（L1 聚合、[PROPOSED]）的事，**不在本 direction 范围**——AC 只要求 moderation → admin 车道 + 非 moderation pass-through。
- **触发器形态 = 「并行新增」，非「重定向改写」**：落地实现（E8）是新增 `audit_events_governance_enqueue`，**未** DROP/改写 0236 的 `audit_events_snaplink_delivery`（两者并行，'g' < 's' 触发序）；0236 binding RAISE 仍是唯一 abort 路径（E8 门 2 与 0236 各自独立查找，R2 钉）。
- **「403 → status=3 立即 dead、422 → dead ≤1 retry」是 connector 状态机语义**（E10，DB-free 单测），**不是** t11 drill 本体（drill = 封闭端点 ⇒ 永 pending、零终态、retry-forever 姿态）。AC1 把两者合并表述——spec §5 A1 分解为两个可独立执行的 oracle。
- **priority drill 车道常量（BACKLOG=100 / MODERATION=200）≠ governance.rs（10 / 100）**：drill :57/:61 自标 [PROPOSED]（"if B5-3 pins different lane values, adjust these two constants only"）；调和属 B5-3，**不是本 direction 的验收**（§7）。

## 2. Verified current state

```
producer（已在，同事务，零改动）          crates/aero-ai/src/worker/mod.rs:372-404（LOCAL_ACTION_MODERATED @401）
                                          crates/aero-im-core/src/service/messages.rs:620-644（:636 workspace.map）
                                          crates/aero-storage/src/message/events.rs:267,337（soft_delete_outboxed_system
                                          → AuditRepo::append_in_tx 同 tx）
                                          crates/aero-storage/src/message/crud.rs:398 · message_reports.rs:305（同 token）

映射权威（已在，untracked ⚠️，6/6 单测实跑绿）  crates/aero-ai/src/governance.rs（E7）

0236 v1 先例（已在）                    migrations/0236_snaplink_governance_reconciliation.sql:68,129-133

0239 DDL + 触发器（✅ 已落地，untracked ⚠️）  migrations/0239_audit_governance_outbox.sql（E8）
0240 B5-3 索引（已落地，untracked ⚠️）       migrations/0240_audit_governance_due_prio_idx.sql（E1，出范围）

connector 消费方（已在，untracked）      crates/aero-audit-connector/（pg.rs SQL 指名表/列 + 3 drill + 24 非 ignored 单测 + 1 ignored PG 测试 + 403/422 dead 语义，E2/E5/E10）

provision Q3（已在，untracked）          crates/aero-eng/src/audit_provision.rs（G0239_CANDIDATES :26 + Q3 四桶 :48/:148/:222 + dead 优先 verdict :109-112，E4）

sibling storage db_tests（已在，untracked）  crates/aero-storage/src/audit_governance.rs（5 个 db_test，E11）

boot 降级（既定）                        crates/aero-server/src/bin/main.rs:245-261（F13：logged claim errors 不 crash，E10）

integration 门（已在）                   scripts/test-integration.sh:147-149,243-457 + scripts/b5-pin.sh:37-44（37 槽）
                                          （全部 `[ -f migrations/0239_audit_governance_outbox.sql ]` 门控，E9）
```

**本 direction 关闭的剩余项**：0239/0240/governance.rs/connector/audit_provision.rs 全部 **untracked**——提交 + 全链验收（三 drill 翻 PASS、`audit_governance::`/`moderation_finalize_outbox_parity` 槽翻 PASS、Q3 四桶可观测、37/37 pin 绿）。direction 描述的「relay claim 报错、drill SKIP、Q3 不跑」在未迁移库上仍是真实前置条件——**验收一律在 throwaway 已迁移库上做**（AGENTS.md §4.3）。

## 3. Scope

**In scope（本 direction，module `crates/aero-ai/src` + 共享 DDL 契约）**：
- 钉死并验证 0239 DDL/触发器契约（R1/R2）——表列/默认/CHECK/due 索引；`message.moderated` → 同事务 v2 行（class='admin'、priority=100、status 0、event_id 1:1）；未知 token pass-through 零 RAISE。
- 验收 oracles 全量可执行：t11 / relay / priority 三 drill + connector 24 非 ignored + 1 ignored PG 测试 + `audit_governance::` / `moderation_finalize_outbox_parity` integration 条目 + provision Q3 四桶 + b5-pin 37/37 guard。
- 提交 untracked 的 `crates/aero-ai/src/governance.rs`（映射权威，必须随本 direction 落库）。
- `cargo test -p aero-ai governance::` 6/6 保持绿（实跑已证）。

**Out of scope**：
- `claim_due` 的 `ORDER BY priority DESC` + 反饥饿上限 → **B5-3**（priority drill 的 PASS 归 B5-3；0240 索引已落地但排序语义是 B5-3 的验收；本 direction 只保证 drill 的**列探测不 SKIP**）。
- 配给门 boot 接线（verdict 进 boot/readiness）→ **B5-4**（其 Q3 分布/status=3 断言依赖本 direction 的表，作为 oracle 在本 spec 验收；boot 强制属 B5-4）。
- L1 聚合（高容量 message.* 窗口，[PROPOSED]）→ campaign 决策；本 direction 只钉 **admin 类永不聚合**（P2 parity，R4）。
- v1/v2 共存决策与 0236 重定向改写 → 本 direction 的并行触发器形态保证 0236 行为零变化（R2）。
- `AuditGovernanceOutboxRepo` 仓储实现与 db_tests → sibling aero-storage slice（E11 已存在，本 spec 只消费其测试名）。
- `AiWorker`/`messages.rs`/connector/`main.rs` 任何代码改动：**禁止**（producer 调用形状冻结，E10）。

## 4. Requirements

### R1 — 0239 迁移：`audit_governance_outbox` DDL（消费者契约全集，已落地 → 钉死）
文件 `migrations/0239_audit_governance_outbox.sql`（文件名被 `scripts/test-integration.sh` 多处门控 + b5-pin 槽钉死，E1/E9）。已落地实现（E8）必须满足：

| 列 | 类型 | 约束 | 消费者钉点 |
|---|---|---|---|
| `event_id` | uuid | PRIMARY KEY（= `audit_events.id`，1:1 幂等键 + 出站 Idempotency-Key） | pg.rs claim/settle/requeue/mark_dead 全部 `WHERE event_id = $1`；relay-drill parity |
| `payload` | jsonb | NOT NULL + CHECK object | pg.rs RETURNING payload → 出站 POST 体 |
| `status` | integer | NOT NULL DEFAULT **0**；CHECK `IN (0,1,2,3)` | pg.rs:26-29 规范值；t11/relay/priority drill 断言 0/2/3 |
| `class` | text | NOT NULL DEFAULT **'message'** | t11/relay drill 种子 INSERT 不含此列 ⇒ 默认必需；priority drill 列探测 + 写 'admin'/'message' |
| `priority` | smallint | NOT NULL DEFAULT **10**（= `GOVERNANCE_PRIORITY_BACKLOG`，governance.rs:33 doc「0239 column default」） | 同上默认必需；moderation 行显式 100 |
| `delivery_mode` | text | NOT NULL DEFAULT 'push'（保留，无消费者） | 0239 文件头注释自述（design-doc promised shape） |
| `available_at`/`created_at` | timestamptz | NOT NULL DEFAULT `clock_timestamp()` | pg.rs claim 过滤 + ORDER BY 第二/三键（单时钟域） |
| `attempts` | bigint | NOT NULL DEFAULT 0 | t11 drill `SUM(attempts)` 逐轮 N/2N；requeue/mark_dead fence |
| `claim_token`/`lease_expires_at` | uuid/timestamptz | 同 NULL 或同非 NULL（claim-state CHECK） | settle/requeue/mark_dead 栅栏 |
| `delivered_at`/`last_error` | timestamptz/text | NULL | settle 置钟；t11 drill `LIKE '%audit connector HTTP transport failed%'` |

due 索引 `(available_at, created_at, event_id) WHERE status IN (0, 1)`（镜像 0235:187 先例；priority DESC 变体 = 0240，B5-3）。幂等（`IF NOT EXISTS` + `DROP TRIGGER IF EXISTS` 先例）。**落地顺序钉死（AGENTS.md §4.2）：先 `cargo build` 再 `aero-cli migrate`**——迁移编译期嵌入 bin，漏 build 即静默 no-op、drill 仍 SKIP。

### R2 — `audit_events` AFTER INSERT 触发器：同事务 enqueue（并行新增，不改 0236）
已落地 `aero_enqueue_governance_audit()` + `audit_events_governance_enqueue`（E8）必须满足：
- `NEW.action = 'message.moderated'` → `INSERT (event_id, status, class, priority, payload) VALUES (NEW.id, 0, 'admin', 100, <envelope>)`——字面量 0/'admin'/100 旁有 **cross-slice pin 注释**（↔ governance.rs `GOVERNANCE_CLASS_ADMIN`/`GOVERNANCE_PRIORITY_MODERATION`/`LOCAL_ACTION_MODERATED`/`MODERATION_OUTBOUND_ACTION`；aero-storage 生产代码不得 import aero-ai，文本 pin 即 drift guard）。
- 其余 action → `RETURN NEW`，零行、**零 RAISE**（`governance_lane_for` 的 `None` 分支是唯一映射面；无第二 abort 路径，R-D2）。
- 映射**仅键于 action token**，不读调用者身份（token-keyed，E5 producer 同形）。
- 门语义保持：runtime disabled ⇒ 零 outbox 行不 raise（`moderation_finalize_runtime_disabled_commits_1_plus_0` 钉）；binding 缺失 ⇒ 本触发器内查找 RAISE 中止整事务（fail-closed，独立于 0236 触发序）。
- `ON CONFLICT (event_id) DO NOTHING`（重放/与 sibling 重定向幂等，`duplicate_event_id_is_deduped_by_on_conflict` 钉）。
- **不得** DROP/改写 `audit_events_snaplink_delivery`（v1 继续流动；共存面由 `non_moderation_action_passes_through_unmapped` 的 0-v2-row 断言间接钉）。

### R3 — 原子性（同事务三件套）
moderation finalize 的单个 PG 事务 = 软删 UPDATE + `AuditRepo::append_in_tx`（audit 行含本地 token 'message.moderated'）+ **AFTER INSERT 行级触发器产出的 outbox 行**（在插入事务内执行，无需 Rust 侧改动）+ `RoomEvent::Deleted` event outbox；任一失败（含 binding RAISE）→ 全回滚。无 post-commit best-effort enqueue。`handle_moderate` 的 `Err` 传播 → 重试 → DLQ 语义不变（worker/mod.rs:422）。

### R4 — P2 parity 与 pass-through（1:1，永不聚合）
- admin 类（`message.moderated`）：`COUNT(audit_governance_outbox) == COUNT(audit_events WHERE action='message.moderated')`，`event_id = audit_events.id` 1:1（`moderation_finalize_outbox_parity` 钉；`is_admin_class` = L1 bypass 分类权威，本 direction 无 L1 窗口 ⇒ parity 平凡成立）。
- 非 moderation action：v2 零行、事务不因本触发器中止（PG 面 `non_moderation_action_passes_through_unmapped`；单元面 `unknown_local_token_passes_through_unmapped` + `user_delete_token_stays_out_of_admin_lane`——`message.deleted` 绝不进 admin 车道，R-D2 负钉）。
- `message.moderated` 重放/重复插入：`ON CONFLICT DO NOTHING` 去重（`duplicate_event_id_is_deduped_by_on_conflict`）。

### R5 — 消费方零改动 + 提交状态
relay、pg.rs SQL、三 drill、`main.rs`、governance.rs 逻辑一律**不改**。交付 = 提交 untracked 文件（0239、0240、`crates/aero-ai/src/governance.rs`、`crates/aero-audit-connector/`、`crates/aero-eng/src/audit_provision.rs` + tests、`crates/aero-storage/src/audit_governance.rs`、drills、b5-pin/test-integration 改动——按 batch 提交规范归组；governance.rs 是**本模块**的映射权威，随本 direction 入库）。`git diff`（相对已提交基线）除本 batch 文件外应为空。

## 5. Acceptance checks（direction 四条原样保留，逐条 testable）

> 前置（每个 throwaway 库验证）：`cargo build`（迁移编译期嵌入）→ 全新一次性库 `CREATE DATABASE` → `aero-cli migrate` 全链 replay（AGENTS.md §4.2/4.3）。

### A1 — 「T-11：aero-audit-t11-drill exits 0 against a throwaway migrated DB（currently SKIP/exit 2）with 403 → status=3 immediate dead and 422 → dead after ≤1 retry」

方向原文保留；分解为两个独立可执行 oracle（E10 勘误：403/422 语义在 connector 状态机，不在 drill 本体）：

- **Oracle A（drill 翻转）**：`bash scripts/test-integration.sh` 的 `t11-fail-closed` 段（throwaway 库 → migrate → drill）→ `b5_check "t11-fail-closed" "PASS"`（不再 `SKIP (0239 not landed)`）；drill 自身 exit 0 且末行 `drill: t11-pending: PASS`（t11-drill.rs:203）。手工复跑：`DATABASE_URL=<throwaway> cargo run -p aero-audit-connector --bin aero-audit-t11-drill`。反向断言：`SELECT to_regclass('audit_governance_outbox')::text` 非 NULL、`ls migrations/0239_audit_governance_outbox.sql` 存在。
- **Oracle B（403/422 dead 语义，DB-free 非 ignored）**：`cargo test -p aero-audit-connector --test state_machine forbidden_dead_on_first_attempt permanent_error_dead_after_exactly_two_attempts` → 全绿。`forbidden_dead_on_first_attempt`（state_machine.rs:231）：stub `events_status: 403` → 首 attempt 即 `FakeStatus::Dead`、posts==1、无 requeue（relay.rs:190-204 钉）；`permanent_error_dead_after_exactly_two_attempts`（:145）：`events_status: 422` → attempt 1 requeue、attempt 2 Dead（`is_dead_at` :48-49，relay.rs:208-220 钉）。
- **Oracle C（DB 级 status 转移全集）**：`cargo test -p aero-audit-connector pg::concurrent_double_claim_across_two_sessions_is_impossible -- --ignored`（`DATABASE_URL` 门控；真 0239 表上 50 行两 session 各 25、零交集、attempts 恒 1、token 全不同）——claim 面在真表上的端到端证据。

### A2 — 「Inserting one audit_events row fires the 0239 redirect trigger and produces an outbox row with status=0, correct class, and governance priority（message.moderated → class='admin', priority from governance_lane_for）；test-integration.sh B5 gate no longer reports the drill as skipped」

- **Oracle A（同事务 + 列值）**：`moderation_finalize_outbox_parity`（sibling slice db_test，audit_governance.rs:211，经 `run_migrated_integration "moderation_finalize_outbox_parity"` 空过滤守卫执行）：BLOCK verdict 走 `AiWorker::handle_moderate` 生产 seam → 恰 1 条 `audit_events` 行（action='message.moderated'）+ 恰 1 条 outbox 行：`status=0`、`event_id=audit_events.id`（1:1）、`class='admin'`、`priority=100`、payload 含 event_id/source_system/action（`admin.content.flag`）。
- **Oracle B（DDL 默认/CHECK）**：`ddl_contract_defaults_and_checks`（:382）：列默认（class 'message'/priority 10/status 0）、CHECK 0..3、claim-state CHECK 可满足。
- **Oracle C（priority 来源 = governance_lane_for）**：`cargo test -p aero-ai governance::` 6/6 绿（实跑已证）——`governance_lane_for("message.moderated") == {class:'admin', priority:100, outbound:'admin.content.flag', status:0}`（`outbound_action_is_single_contract_token` 钉）；0239 触发器字面量 100 由 cross-slice pin 注释对齐（R2，文本 grep：`grep -n "cross-slice pin" migrations/0239_audit_governance_outbox.sql` ≥4 处）。
- **Oracle D（B5 gate 翻转）**：`bash scripts/test-integration.sh` → B5 日志出现 `B5-CHECK audit_governance::: PASS` 与 `B5-CHECK moderation_finalize_outbox_parity: PASS`（不再 `SKIP (0239 not landed)`）；`a3-relay-drill` 同样 PASS（relay-drill:164 `PASS: {N}/{N} delivered (status 2), event_id set-parity exact, stub POSTs {N}`）。
- **Oracle E（runtime 门保持）**：`moderation_finalize_runtime_disabled_commits_1_plus_0`（:482）：runtime disabled ⇒ 1 audit 行 + **0** outbox 行（0236 门语义无回归）。

### A3 — 「P2 parity：for admin-class rows COUNT(audit_governance_outbox) == COUNT(audit_events) with event_id 1:1（never merged by L1）；non-moderation rows keep flowing through the shared redirect（governance.rs unknown-token pass-through）」

- **Oracle A（parity）**：`moderation_finalize_outbox_parity` 的计数断言（A2 Oracle A 同源）：admin 类 outbox 行数 == audit_events 行数，`event_id` 逐一相等——本 direction 无 L1 窗口，parity 必须平凡成立（`is_admin_class` :102-106 即「永不聚合」分类钉）。
- **Oracle B（pass-through PG 面）**：`non_moderation_action_passes_through_unmapped`（:527）：`action='room.create'` 等非 moderation 行事务内提交 → 0 条 v2 outbox 行、事务不因本触发器中止（无第二 RAISE）。
- **Oracle C（pass-through 单元面 + R-D2 负钉）**：`cargo test -p aero-ai governance::`：`unknown_local_token_passes_through_unmapped`（room.create/message.create/message.edit/message.deleted/call.join/"" 全部 `None`）+ `user_delete_token_stays_out_of_admin_lane`（`message.deleted` 绝不进 admin 车道）+ `mapping_is_token_keyed`（同 token ⇒ 同 tuple，与调用者无关）。
- **Oracle D（幂等去重）**：`duplicate_event_id_is_deduped_by_on_conflict`（:586）：同 event_id 二次插入被 `ON CONFLICT DO NOTHING` 吞掉，行数不变。

### A4 — 「37/37：the pinned contract list in scripts/test-integration.sh:143（b5-pin.sh）gains a green 0239 row；audit-provision-check Q3 reports four buckets with dead never folded into delivered」

- **Oracle A（pin guard）**：`bash scripts/test-b5-pin-guard.sh` → guard PASS（恰 37 槽：15 执行 + 22 `[PROPOSED]`；无重复/无 malformed/非空转）；`bash scripts/test-integration.sh` 末尾打印 `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS` 且每个执行槽（含 `audit_governance::`、`moderation_finalize_outbox_parity`、`a3-relay-drill`、`t11-fail-closed`、`moderation-priority-drill`、`audit-provision-check`）在 `$B5_LOG` 有 verdict 行——「0239 行」= `audit_governance::` + `moderation_finalize_outbox_parity` 两槽从 SKIP 翻 PASS。
- **Oracle B（Q3 四桶）**：throwaway 迁移库上 `DATABASE_URL=… AERO__DATABASE__URL=… cargo run -p aero-cli -- audit-provision-check` → 输出含 `outbox-0239: table=audit_governance_outbox pending=<n> claimed=<n> delivered=<n> dead=<n>`（四桶各自成行，缺桶补 0，audit_provision.rs:148/:222）；**dead 永不折入 delivered**：`dead > 0` ⇒ `Verdict::FailClosed("…dead is never counted delivered…")`（:109-112）——单元面 `cargo test -p aero-eng audit_provision` 的 `verdict_dead_is_fail_closed_even_with_relay_on` / `verdict_dead_priority_over_relay_disabled` 钉此优先级（dead 优先于 relay 健康态）。
- **Oracle C（Q3 在真表上出四桶的集成面）**：test-integration.sh 的 provision leg B1（空库一致 exit 0）与 leg D/C（`UPDATE audit_governance_outbox SET status = 3` 后断言 `outbox-0239: … dead=…`）在 0239 已迁移库上执行且 `b5_check "audit-provision-check" "PASS"`。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| drill 翻转（A1 Oracle A / A2 Oracle D / A4 Oracle A） | `crates/aero-audit-connector/src/bin/{aero-audit-t11-drill,aero-audit-relay-drill,aero-audit-priority-drill}.rs`（to_regclass 探测 + 断言） | integration：throwaway 库 + 全链迁移 + stub sink；`scripts/test-integration.sh:305-457`（0239 文件门控解锁）+ `scripts/test-b5-pin-guard.sh` |
| 403/422 dead 语义（A1 Oracle B） | `crates/aero-audit-connector/tests/state_machine.rs`（`forbidden_dead_on_first_attempt` :231 / `permanent_error_dead_after_exactly_two_attempts` :145）+ `src/relay.rs`（`deliver_claim` :173-226 / `is_dead_at` :48-49） | `cargo test -p aero-audit-connector`（非 ignored，无 DB） |
| 同事务 parity + 列值（A2 Oracle A/B/E、A3 Oracle A/B/D） | `crates/aero-storage/src/audit_governance.rs` db_tests（`moderation_finalize_outbox_parity` :211 / `ddl_contract_defaults_and_checks` :382 / `moderation_finalize_runtime_disabled_commits_1_plus_0` :482 / `non_moderation_action_passes_through_unmapped` :527 / `duplicate_event_id_is_deduped_by_on_conflict` :586） | `run_migrated_integration`（`--test-threads=1`，空过滤守卫），`DATABASE_URL` + 已迁移 |
| pass-through 单元 + 映射权威（A2 Oracle C / A3 Oracle C） | `crates/aero-ai/src/governance.rs` tests（6 项） | `cargo test -p aero-ai governance::`（无 DB；实跑 6/6 绿） |
| claim 面真表（A1 Oracle C） | `crates/aero-audit-connector/src/pg.rs::concurrent_double_claim_across_two_sessions_is_impossible`（+ `ensure_outbox_table` 回退） | `cargo test -p aero-audit-connector -- --ignored`（`DATABASE_URL`） |
| Q3 四桶 + dead 优先（A4 Oracle B/C） | `crates/aero-eng/src/audit_provision.rs`（Q3_SQL :48 / parse_buckets :222 / verdict :109-112）+ `crates/aero-eng/tests/audit_provision.rs`（`verdict_dead_is_fail_closed_even_with_relay_on` :106 / `verdict_dead_priority_over_relay_disabled` :136） | `cargo test -p aero-eng audit_provision`（无 DB）+ `aero-cli audit-provision-check`（throwaway 迁移库）+ test-integration.sh provision legs |
| 提交门禁 | governance.rs 等 untracked 文件入库；`git diff` 除本 batch 外为空（R5） | review + `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 |

## 7. Risks / [PROPOSED]

- **全 batch 产物 untracked（最高风险）**：0239/0240、`crates/aero-audit-connector/`、`crates/aero-ai/src/governance.rs`、`crates/aero-eng/src/audit_provision.rs`(+tests)、`crates/aero-storage/src/audit_governance.rs`、drills、b5-pin/test-integration 改动均未提交——任何一次 `git reset --hard master`（AGENTS.md §4.1 多 agent 并行流程）都会抹掉本 direction 的全部验收对象。**提交是 A1-A4 全部 oracle 成立的前提**。
- **priority drill 的 PASS 归 B5-3**：0239 落库后 drill 不再 exit 2 SKIP，但 `ORDER BY` 无 priority 项（pg.rs:87）→ round-1 `MIN(delivered_at)` 断言 FAIL 红——这是 priority-drill.rs:28-33 明示的**诚实信号**，不是本 direction 的回归；`moderation-priority-drill` 槽在 B5-3 落地前保持红（或 SKIP-with-reason，视 harness 排期）。
- **priority drill 车道常量漂移（100/200 vs 10/100）**：drill :57/:61 自标 [PROPOSED]；B5-3 调和。本 direction 只钉列默认 10（drill 种子行显式带 priority，不依赖默认）。
- **0240 已落地但属 B5-3**：`migrations/0240_audit_governance_due_prio_idx.sql` 是 B5-3 的索引（priority DESC），本 direction 验收不依赖它（A1 Oracle C 用 FIFO 语义的 0239 due 索引即可）；提交时与 0239 同批（rolling-deploy 共存设计，0240 头注释自述）。
- **迁移编译期嵌入**：漏 `cargo build` 直接 `aero-cli migrate` → 0239 静默 no-op、drill 仍 SKIP——A1 的「反向断言」（文件存在 + to_regclass 非 NULL）防此坑。
- **「37-tests」为仓外契约标签**：本 spec 以 in-repo 可执行读法（15 执行槽 + b5-pin guard + 具体测试名）落实；契约原文若冲突，改的是标签映射，不是测试。
- **AC1 的表述合并**：direction 把 drill exit 0 与 403/422 dead 语义写进同一条——两者分属 drill 本体（DB 面）与 connector 状态机（无 DB 面），本 spec §5 A1 已分解；若只跑 drill 而忽略 Oracle B，403/422 语义无覆盖。

## 8. Sequencing / verification

1. **提交 batch 产物**（前置，§7 最高风险）：0239 + 0240 + governance.rs + connector + audit_provision + audit_governance + drills + scripts 改动。
2. **快速 oracle**：`cargo test -p aero-ai governance::`（6/6）→ `cargo test -p aero-audit-connector`（24 非 ignored）→ `cargo test -p aero-eng audit_provision` → `cargo test -p aero-audit-connector -- --ignored pg::concurrent_double_claim…`（需 DATABASE_URL）。
3. **全链 integration**：`cargo build` → `bash scripts/test-integration.sh`——`audit_governance::`/`moderation_finalize_outbox_parity`/`a3-relay-drill`/`t11-fail-closed`/`audit-provision-check` 槽全 PASS；`moderation-priority-drill` 槽红或 SKIP-with-reason（B5-3 排期）；pin guard `37/37 PASS`。
4. **Q3 手工面**：throwaway 迁移库上 `aero-cli audit-provision-check` → `outbox-0239: table=audit_governance_outbox pending=0 claimed=0 delivered=0 dead=0`（空库四桶），leg D/C 场景 dead 桶非零且 verdict fail-closed。
5. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规。
6. **B5-3/B5-4 交接**：priority drill 翻 PASS 归 B5-3（0240 索引 + claim ORDER BY priority DESC + 反饥饿）；provision gate 的 boot 强制归 B5-4（本 direction 只保证其 Q3 可观测）。
