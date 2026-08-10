# Requirements Spec — B5-1 0239 landing + same-tx governance enqueue（module: crates/aero-audit-connector）

- **Module (analysis root)**: `crates/aero-audit-connector`（consumer 侧；DDL 落在 `migrations/`，映射权威在 `crates/aero-ai/src/governance.rs`，同事务 enqueue 为 0239 触发器）
- **Direction**: "Land B5-1 0239 (audit_governance_outbox DDL: status 0/1/2/3 + class/priority, stamped from aero_ai::governance) + the same-tx enqueue path for message.*/room.*/admin.* writes"（value 10 / risk_reduction 9 / effort 8 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-audit-connector-40d338d1.json` (direction #0)
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（v2 契约正文在仓外，[PROPOSED]）
- **Status**: Requirements（下述证据全部经源码 grep + **实跑**核对——含 throwaway 已迁移库上的三个 drill 与 storage db_tests 全绿；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

> ⚠️ **关键状态校正（本 spec 与 analysis 的最大差异）**：analysis 生成时（2026-08-07 14:43）`migrations/` 尾号为 0238、0239 不存在、drills 全 SKIP；**核对时（2026-08-08）工作树已含完整落地**——`migrations/0239_audit_governance_outbox.sql`（DDL + 并行触发器）+ `0240`（B5-3 索引）+ `0241`（disabled-window reconciler）、`crates/aero-audit-connector/` 全 crate、`crates/aero-ai/src/governance.rs`、`crates/aero-storage/src/audit_governance.rs`（6 db_tests）、`crates/aero-eng/src/audit_provision.rs`、drills、`scripts/b5-pin.sh` + test-integration 改动全数存在（均 **untracked**）。本 direction 描述的空缺已被工作树闭合；**本 spec 的职责 = 把 DDL/触发器契约钉死为可验证需求 + 验收 oracles 全部可执行 + 把「未提交」列为必须关闭的交付项**。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `migrations/`（"tops out at 0238_message_recall.sql"） | ⚠️ **已过时（核心校正）**。tracked 尾号确为 `0238_message_recall.sql`，但工作树 untracked 已有 `0239_audit_governance_outbox.sql`、`0240_audit_governance_due_prio_idx.sql`、`0241_governance_reconcile.sql`（`git status --short migrations/` 三个 `??`）。文件名被 in-repo 消费者钉死：`scripts/test-integration.sh:306/:325/:351/:447` 全部 `[ -f "migrations/0239_audit_governance_outbox.sql" ]` 门控（else 分支显式 `b5_check … SKIP (0239 not landed)`）。**0240/0241 属 B5-3/sibling，非本 direction 交付**（0239 头注释自述："the index change is B5-3's, not this slice's"）。 |
| E2 | `crates/aero-audit-connector/src/pg.rs`（claim_due/settle/requeue/mark_dead over `audit_governance_outbox`, STATUS 0..3） | ✅ **Verified**。`STATUS_ENQUEUED=0/CLAIMED=1/DELIVERED=2/DEAD=3` 常量 :28-31（doc："**B5-1 0239 DDL normative values**"）。`claim_due` :69-108：`FROM audit_governance_outbox`、`status IN (0,1)`、`available_at <= clock_timestamp()`、`FOR UPDATE SKIP LOCKED`、`LIMIT` clamp `[1,500]`；ORDER BY 现含 `priority DESC` 首项（:87-88）——B5-3 fold-in 已随工作树落地（0240 索引配套）。`settle` :110-153（fenced → status=2 + delivered_at）、`requeue` :155-187（status=0 + backoff）、`mark_dead` :189-217（status=3）。**单时钟域**：全用 `clock_timestamp()`，trait 无 caller `now`（防 app/DB 时钟偏移 livelock）。`reconcile` 调 `aero_reconcile_governance_audit($1)`（0241 函数）。 |
| E3 | 三 drill 的 exit-2 SKIP 探测（`aero-audit-{t11,relay,priority}-drill.rs`） | ✅ **Verified（探测存在；且已实跑翻绿）**。t11-drill:73-83 / relay-drill:57-67 / priority-drill:87-118：`SELECT to_regclass('audit_governance_outbox')::text` 为 NULL → `eprintln!(SKIP …)` + `exit(2)`；priority-drill 另探测 `priority`/`class` 列（:105-118，缺列 SKIP、缺排序 FAIL 红）。**实跑（throwaway 已迁移库，本 spec 亲自执行）**：t11 → `round 2: 3/3 pending, 0 terminal, attempts sum 6 … drill: t11-pending: PASS`（exit 0）；relay → `PASS: 3/3 delivered (status 2), event_id set-parity exact, stub POSTs 3`（exit 0）；priority → `moderation-in-first-batch: PASS` + `drain-501: PASS` + `parity-501: PASS`（exit 0）。**t11/relay 种子 INSERT 不含 class/priority 列**（t11:127-132 / relay:98-103）⇒ 两列默认值（'message'/10）被实跑验证必需且已满足。 |
| E4 | `crates/aero-server/src/bin/main.rs:250-267`（relay 已 spawn，presence-gated） | ✅ **Verified（行号微漂：:245-266）**。`RelayConfig::from_env()` presence-gated：`Ok(Some(cfg))` → `PgOutboxRepo::new(persistence.pg.clone())` + `AuditClient::new` + `AuditRelay::new` → `tracker.spawn(relay.spawn(ai_shutdown.clone()))`；注释原文 **"booting before the 0239 table lands degrades to logged claim errors, not a crash"**（F13 既定降级，非缺口）。`Ok(None)` 静默；`Err` fail-loud boot 错误。 |
| E5 | `crates/aero-storage/src/audit.rs`（append_in_tx 存在；write 路径 post-hoc best-effort） | ✅ **Verified（部分过时）**。`AuditRepo::append_in_tx` :127（事务内 append，ROADMAP 审计事务化）。`routes/handlers/auth.rs:55/:217` 确为 post-hoc best-effort（`let _ = …AuditRepo::new(…).append(…)`，注释 "best-effort — never block"）；`crates/aero-server/src/channels.rs` 亦引用 AuditRepo。**但同事务 append 在 storage 层已有 in-tx callers**：`message/events.rs:337`（`soft_delete_outboxed_system` 内）、`message/crud.rs:374/:407`（`soft_delete_audited`/`soft_delete_moderated`）——direction 所述 "zero in-tx callers" 不确；**真正缺口（无 outbox enqueue）由 0239 触发器闭合，非 Rust 代码**（设计 C4：AFTER INSERT row trigger，零 Rust 生产改动）。 |
| E6 | `crates/aero-audit-connector/src/outbox.rs`（无 enqueue_in_tx — [PROPOSED]） | ✅ **Verified（仍成立）**。trait 现含 `reconcile` / `claim_due` / `settle` / `requeue` / `mark_dead` 五方法，**无 enqueue 方法**。同事务 enqueue 由 0239 的 `aero_enqueue_governance_audit()` 触发器承担（设计 C4 决议：SQL 触发器 = 同事务 enqueue 机制，Rust 侧零改动）；**Rust 侧 `enqueue_in_tx` 仍属 [PROPOSED]**（L1 聚合方向），不在本 direction 范围（§3）。 |
| E7 | `crates/aero-ai/src/governance.rs:31-33,86-104`（映射权威） | ✅ **Verified（实跑 6/6 绿）**。`GOVERNANCE_PRIORITY_MODERATION: i16 = 100` :31 / `GOVERNANCE_PRIORITY_BACKLOG: i16 = 10` :33（doc 明言 **"also the 0239 column default"**）；`GOVERNANCE_CLASS_ADMIN/MESSAGE/ROOM` :37-41；`LOCAL_ACTION_MODERATED = "message.moderated"` :49；`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` :60；`GovernanceLane` :61-74（class/priority/outbound_action/status）；`governance_lane_for` :86-99（`message.moderated` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`；未知 token → `None` pass-through，永不 raise/block）；`is_admin_class` :102-106（L1 bypass 分类权威）。6 项单测：`moderation_lane_preempts_backlog_under_desc_claim` / `unknown_local_token_passes_through_unmapped` / `user_delete_token_stays_out_of_admin_lane`（R-D2 负钉）/ `mapping_is_token_keyed` / `outbound_action_is_single_contract_token` / `admin_class_rows_never_aggregated`。**实跑**：`cargo test -p aero-ai governance::` → **6 passed; 0 failed**。⚠️ 文件 **untracked**——必须随本 direction 提交（§7 最高风险）。 |
| E8 | （补充）已落地的 0239 内容 | ✅ **Verified**。`CREATE TABLE IF NOT EXISTS audit_governance_outbox`：`event_id uuid PRIMARY KEY`（= audit_events.id，1:1 + 出站 Idempotency-Key）、`status integer NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))`、`class text NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`、`priority smallint NOT NULL DEFAULT 10 CHECK (priority > 0)`、`delivery_mode text NOT NULL DEFAULT 'push' CHECK (delivery_mode IN ('push'))`（保留，零消费者）、`payload jsonb NOT NULL CHECK (jsonb_typeof(payload)='object')`、`available_at/created_at timestamptz DEFAULT clock_timestamp()`、`attempts bigint DEFAULT 0 CHECK (>=0)`、`claim_token uuid`、`lease_expires_at`、`delivered_at`、`last_error`、claim-state CHECK（token/lease 同 NULL 或同非 NULL，镜像 0235:178-181）。due 索引 `audit_governance_due_idx (available_at, created_at, event_id) WHERE status IN (0,1)`。函数 `aero_enqueue_governance_audit()`：runtime 门（disabled ⇒ RETURN NEW 零行，镜像 0236:75-79）→ token 门（`action <> 'message.moderated'` ⇒ RETURN NEW pass-through，**零 RAISE**）→ binding 查找（缺失 RAISE 中止整事务，fail-closed）→ `INSERT (event_id,status,class,priority,payload) VALUES (NEW.id, 0, 'admin', 100, jsonb_build_object(…))`（envelope：event_id/source_system/event_type/schema_id/schema_version/occurred_at/actor/targets/aggregate_type/aggregate_id/action='admin.content.flag'/outcome/payload/data_classification/retention_class/idempotency_key）+ `ON CONFLICT (event_id) DO NOTHING`。触发器 `audit_events_governance_enqueue AFTER INSERT ON audit_events FOR EACH ROW`。**cross-slice pin 注释**齐备（列默认 ↔ governance.rs:33/:34；moderation 戳 ↔ :31/:33/:60，键 :49）。 |
| E9 | （补充）integration 门控锚点 | ✅ **Verified**。`scripts/test-integration.sh`：:306-318 `audit_governance::` + `moderation_finalize_outbox_parity` 两条 `run_migrated_integration`（**空过滤守卫**：`grep -Eq 'test result: ok\. [1-9][0-9]* passed'`，防 vacuous green）；:325-340 A3 relay drill 段；:347-397 T-11 drill 段（含 B5-4 provision leg D/C：`UPDATE audit_governance_outbox SET status = 3` + `outbox-0239: table=… pending=…` 断言）；:441-461 moderation-priority drill 段（0239 文件门控，PASS/FAIL 语义见 E3）。`scripts/b5-pin.sh`：恰 37 槽（15 执行 + 22 `[PROPOSED]`），`assert_b5_contract_pin` 守卫（恰 37/无重复/无 malformed/非空转/verdict 行）；`scripts/test-b5-pin-guard.sh` 存在。 |
| E10 | （补充）connector 状态机与套件 | ✅ **Verified（实跑全绿）**。`relay.rs`：`dispatch_batch` :164（**reconcile → claim → for_each_concurrent deliver**）；`deliver_claim` :173-231：Ok→settle；**403 → 首次即 mark_dead**（:190-204，T-11 fail-closed）；permanent（422/409/回执/payload-guard）→ `is_dead_at(attempts)`（:48-49，attempt ≥2）mark_dead 否则 requeue（:208-220）；transient → requeue 永不 dead。`audit_backoff`/`clamped_lease`/`truncate_error` :56-78。**实跑**：`cargo test -p aero-audit-connector` → lib 8 passed + 3 ignored（pg.rs，需 PG）、`claim_validation` 11 passed、`state_machine` 6 passed——非 ignored 全绿。 |
| E11 | （补充）sibling storage slice db_tests | ✅ **Verified（实跑 6/6 绿）**。`crates/aero-storage/src/audit_governance.rs`：`moderation_finalize_outbox_parity` :247（= b5-pin 槽名）、`ddl_contract_defaults_and_checks` :424、`moderation_finalize_runtime_disabled_commits_1_plus_0` :583、`non_moderation_action_passes_through_unmapped` :628、`duplicate_event_id_is_deduped_by_on_conflict` :693、`governance_reconcile_backfills_disabled_window` :774——全部 `#[ignore = "requires live Postgres"]`，经 `run_migrated_integration` 在 throwaway 已迁移库跑。**实跑（throwaway 已迁移库）**：`cargo test -p aero-storage --lib audit_governance:: -- --ignored --test-threads=1` → **6 passed; 0 failed**。 |

### 1.1 对 direction problem 陈述的勘误/钉化（evidence-backed）

- **「表不存在 / 三 drill 全 SKIP / B5 sections 全 SKIP」已闭合**（E1/E3/E8）：工作树已有 0239（+0240/0241 sibling）；本 spec 实跑三 drill 与 6 db_tests **全部 exit 0 / PASS**（E3/E11）。本 direction 的剩余交付 = **提交 + 全链 harness 复跑**（§8）。
- **「message.*/room.*/admin.* 同事务 enqueue」的准确边界**：0239 触发器只映射 `message.moderated`（唯一已钉 token，E7/E8）——message.create/edit/room.* 等**没有** audit 行产出或映射为 `None` pass-through（R-D2 负钉：`user_delete_token_stays_out_of_admin_lane` 钉死 `message.deleted` 绝不进 admin 车道）。**「message.*/room.*/admin.* 全量 in-tx enqueue producer」是 sibling write-half direction（`b5-1-write-half-land-migration-0239-status-0-1-2-b65a1b69`，module `crates/aero-im-core`）**——其 requirements stage FAILED（DECISIONS.md: "stage 'requirements' — FAIL … agent exited 1"）从未落地；本 direction 不重复其范围（§3 红线）。
- **触发器形态 = 「并行新增」，非「重定向改写」**（E8）：`audit_events_governance_enqueue` 与 0236 的 `audit_events_snaplink_delivery` **并行共存**（'g' < 's' 触发序）；0236 binding RAISE 仍是唯一 abort 路径（0239 门 2 各自独立查找）。v1/v2 共存由 `moderation_finalize_outbox_parity` half 4 钉（v1 行仍流，payload 保留本地 token）。
- **「403 → status=3 立即 dead、422 → dead ≤1 retry」是 connector 状态机语义**（E10，DB-free 单测），**不是** t11 drill 本体（drill = 封闭端点 ⇒ 永 pending、零终态、retry-forever 姿态）。AC2 把两者合并表述——spec §5 分解为独立可执行 oracle。
- **priority drill 车道常量（BACKLOG=100 / MODERATION=200）≠ governance.rs（10 / 100）**：drill :57/:61 自标 [PROPOSED]（"if B5-3 pins different lane values, adjust these two constants only"）；**B5-3 fold-in 已随工作树落地**（pg.rs claim ORDER BY `priority DESC` + 0240 索引），priority drill 已 PASS（E3）——但 B5-3 方向仍拥有该语义的正式所有权。
- **`AuditRepo::append_in_tx` 的 in-tx callers 并非零**（E5）：storage 层 `message/events.rs:337`、`message/crud.rs:374/:407` 已同事务；server 路由层确为 post-hoc best-effort（auth.rs:55/:217）。direction 该 claim 过时，不影响验收（enqueue 机制 = 触发器，非 Rust append）。

## 2. Verified current state（实跑证据）

```
producer（已在，同事务，零改动）      crates/aero-ai/src/worker/mod.rs:372-404（LOCAL_ACTION_MODERATED @401）
                                    crates/aero-im-core/src/service/messages.rs:620-644（:636 workspace.map）
                                    crates/aero-storage/src/message/events.rs:267,337（soft_delete_outboxed_system
                                    → AuditRepo::append_in_tx 同 tx）
                                    crates/aero-storage/src/message/crud.rs:398 · message_reports.rs:305（同 token）

映射权威（已在，untracked ⚠️，实跑 6/6 绿）  crates/aero-ai/src/governance.rs（E7）

0239 DDL + 触发器（✅ 已落地，untracked ⚠️）  migrations/0239_audit_governance_outbox.sql（E8）
0240（B5-3 索引）/ 0241（reconciler）        migrations/（untracked，sibling 交付，非本 direction）

connector 消费方（已在，untracked ⚠️）       crates/aero-audit-connector/（pg.rs/relay.rs/outbox.rs + 3 drill + 套件，
                                    实跑非 ignored 全绿 + 三 drill exit 0，E3/E10）

sibling db_tests（已在，untracked ⚠️）       crates/aero-storage/src/audit_governance.rs（6 db_tests 实跑全绿，E11）

boot 降级（既定，非缺口）               crates/aero-server/src/bin/main.rs:245-266（F13：logged claim errors 不 crash）

integration 门（已在）                 scripts/test-integration.sh:306-461 + scripts/b5-pin.sh（37 槽）
                                    + scripts/test-b5-pin-guard.sh（E9）

Q3 可观测（已在）                      crates/aero-eng/src/audit_provision.rs（G0239_CANDIDATES :26 / Q3_SQL :48 /
                                    verdict :109-112 dead 优先 fail-closed）+ aero-cli audit-provision-check 臂
```

**未闭合项**：全部 B5 产物 **untracked**（`git status --short` 70 项：56 `??` + 14 `M`）——本 direction 的验收 oracles 全部依赖这些文件存在；**提交是验收成立的前提**（§7 最高风险）。

## 3. Scope

**In scope（本 direction）**：
- 0239 迁移文件的存在性 + DDL/触发器契约（E8，文件已落地，本 spec 钉死其契约与验收）。
- 验收 oracles 全量可执行：三 drill + `audit_governance::`/`moderation_finalize_outbox_parity` integration 条目 + connector 套件 + governance.rs 单测（§5 A1-A5）。
- 提交 `migrations/0239_audit_governance_outbox.sql` + `crates/aero-ai/src/governance.rs` + `crates/aero-audit-connector/` + sibling `audit_governance.rs` + `audit_provision` + drills + `scripts/{test-integration.sh,b5-pin.sh,test-b5-pin-guard.sh}` 改动（本 batch 唯一阻塞，§7）。

**Out of scope（红线，不得越界）**：
- **message.*/room.*/admin.* 全量 in-tx enqueue producer**（Rust 侧 `enqueue_in_tx`、`send_message`/`edit_message`/`room.create` 等 audit + outbox 行）→ sibling write-half direction（module `crates/aero-im-core`，b65a1b69）——其 requirements FAILED 未落地；本 direction 只保证 **mapped 子集**（`message.moderated` → admin 车道）同事务（0239 触发器）与**未映射 pass-through 零 raise**（R-D2）。
- **L1 聚合车道**（[PROPOSED]）：零代码存在（grep 无聚合实现）；本 direction 只钉分类权威（`is_admin_class` / `admin_class_rows_never_aggregated`——admin 行永不聚合）+ 约束（**永不加 HTTP hop、永不 block 消息事务**）为设计约束，非交付物（§7）。
- **B5-3**（claim ORDER BY priority DESC 的正式所有权、反饥饿、drill 车道常量调和）——fold-in 已随工作树落地但属其方向；**B5-4**（boot 强制 provision gate）——其 Q3 可观测依赖本表，但 boot 强制归其方向。
- connector 任何 Rust 生产代码改动：**禁止**（producer 调用形状冻结，E5/E10）。

## 4. Requirements

### R1 — 0239 迁移存在且 DDL 契约成立（消费者契约全集）
文件 `migrations/0239_audit_governance_outbox.sql` 存在（被 `scripts/test-integration.sh:306/:325/:351/:447` 四处文件门控钉死，E1）。表 `audit_governance_outbox` 列 = 消费方（E2/E3/E8）逐列点名的并集：

| 列 | 类型/约束 | 消费者钉点 |
|---|---|---|
| `event_id` | uuid PRIMARY KEY（= `audit_events.id`，1:1 幂等键 + 出站 Idempotency-Key） | pg.rs 全部 `WHERE event_id = $1`；relay/t11 drill parity；`ON CONFLICT (event_id) DO NOTHING` |
| `status` | integer NOT NULL DEFAULT 0 CHECK (0,1,2,3) | pg.rs:28-31 常量；claim 过滤 `status IN (0,1)`；provision Q3 四桶 |
| `class` | text NOT NULL DEFAULT 'message' CHECK ('admin','message','room') | governance.rs GOVERNANCE_CLASS_*（E7）；priority drill 列探测（E3） |
| `priority` | smallint NOT NULL DEFAULT 10 CHECK (>0) | = GOVERNANCE_PRIORITY_BACKLOG（E7）；claim `ORDER BY priority DESC`（B5-3 fold-in）；drill 列探测 |
| `delivery_mode` | text NOT NULL DEFAULT 'push' CHECK ('push') | 保留（零消费者；B5-3 delivery policy 扩展） |
| `payload` | jsonb NOT NULL CHECK (jsonb_typeof='object') | pg.rs RETURNING payload → 出站 POST 体 |
| `available_at` | timestamptz NOT NULL DEFAULT clock_timestamp() | claim 过滤（E2）；单时钟域 |
| `attempts` | bigint NOT NULL DEFAULT 0 CHECK (>=0) | claim 后自增（pg.rs:97）；t11 SUM(attempts) oracle |
| `claim_token` / `lease_expires_at` | uuid / timestamptz（claim-state CHECK 同 NULL 或同非 NULL） | fenced settle/requeue/mark_dead（E2） |
| `delivered_at` / `last_error` / `created_at` | timestamptz / text / timestamptz DEFAULT clock_timestamp() | settle 戳 / requeue·dead 详情 / claim tiebreak |

**无 FK 到 audit_events——deliberate**（0239 注释 + 设计 doc §4）：retention sweep（boot/retention.rs `sweep_audit`/`sweep_audit_partitions`）按行 DELETE/整分区 DROP，FK 会让 retention 被 relay 进度 hostage；不变量 = "outbox 行 ⟹ 提交时源行存在"（AFTER INSERT 触发器同事务保证，MVCC 原子可见）。

### R2 — 同事务 enqueue 触发器契约（message.moderated → admin 车道）
`audit_events` AFTER INSERT 行触发器 `audit_events_governance_enqueue`（函数 `aero_enqueue_governance_audit()`，0239 内 `CREATE OR REPLACE`，E8）：
- **门 1（fail-open，runtime）**：`snaplink_commercial_runtime.enabled` 关 ⇒ `RETURN NEW` 零 outbox 行（镜像 0236:75-79；`moderation_finalize_runtime_disabled_commits_1_plus_0` 钉 1 audit + 0 outbox）。
- **门 2（token-keyed，fail-open pass-through）**：`action <> 'message.moderated'` ⇒ `RETURN NEW` 零 raise（governance_lane_for(其他) == None，R-D2；`non_moderation_action_passes_through_unmapped` 钉）。
- **门 3（fail-closed，binding）**：`aero_snaplink_binding_for_workspace(NEW.workspace_id)` 缺失 ⇒ RAISE 中止**整事务**（软删 + audit + outbox 同回滚；`moderation_finalize_without_binding_aborts_tx` 钉）。
- **戳记**：class 'admin'、priority 100、payload action 'admin.content.flag'（= governance_lane_for("message.moderated")，E7/E8 cross-slice pin）。
- **幂等**：`ON CONFLICT (event_id) DO NOTHING`（`duplicate_event_id_is_deduped_by_on_conflict` 钉）。
- **并行共存**：不 DROP/改写 0236 的 `audit_events_snaplink_delivery`（v1 行仍流，half 4 钉）。

### R3 — 映射权威单点（governance.rs）
`governance_lane_for` / `is_admin_class` / `LOCAL_ACTION_MODERATED` / `GOVERNANCE_PRIORITY_*` / `MODERATION_OUTBOUND_ACTION` 为 0239 触发器字面量的唯一权威（cross-slice pin 注释文本对齐，`grep -n "cross-slice pin" migrations/0239_audit_governance_outbox.sql` ≥4 处）。6 项单测钉契约（E7）。

### R4 — connector 消费面不改动、状态机契约保持
`pg.rs`（claim/settle/requeue/mark_dead/reconcile）、`relay.rs`（403 首次即 dead / permanent ≥2 dead / transient 永不 dead）、`outbox.rs` trait 五方法——**本 direction 不改任何一行**；0239 落库后这些路径从"降级报错"转正常（F13 语义翻转）。

### R5 — 提交（本 direction 的关闭条件）
上述全部 untracked 文件 + `scripts/` 改动 + `Cargo.toml`（connector crate 注册）入库；提交后 `cargo check --workspace` 干净。

## 5. Acceptance（direction 验收逐条保留 + 可测试化）

### AC1 — 「Fresh-DB `aero-cli migrate`（throwaway DB replay, make migrate-smoke）creates 0239 with status/class/priority」

- **Oracle A（migrate 全链）**：`cargo build`（迁移编译期嵌入，AGENTS.md §4.2 顺序：build → migrate）→ `make migrate-smoke`（`scripts/migrate_chain_smoke.sh` throwaway 全链 replay）→ exit 0；或 test-integration.sh 任意 0239 门控段（每段先 `aero-cli migrate` 再断言）。
- **Oracle B（表形态）**：throwaway 库上 `SELECT to_regclass('audit_governance_outbox')::text` 非 NULL；`information_schema.columns` 含 `status`/`class`/`priority`（priority-drill:105-118 探测面）。
- **Oracle C（列默认/CHECK，DB 级）**：`ddl_contract_defaults_and_checks`（audit_governance.rs:424，实跑 PASS，E11）：status 默认 0、class 默认 'message'、priority 默认 10、CHECK 0..3 / class 三值 / priority>0、claim-state CHECK 可满足。

### AC2 — 「test-integration.sh B5 sections flip SKIP→PASS：`audit_governance::` db_tests 与 `moderation_finalize_outbox_parity` green（37-tests named entries），a3-relay-drill green（COUNT(status=2)==N, event_id set-parity），t11 drill green（T-11: N rows stay status=0 across 2 rounds, SUM(attempts)=2N, 0 terminal rows, last_error records transport failure — never falsely delivered/dead）」

- **Oracle A（drill 翻转，实跑已证）**：`bash scripts/test-integration.sh` → B5 日志 `B5-CHECK a3-relay-drill: PASS`（relay-drill:174 `PASS: {N}/{N} delivered (status 2), event_id set-parity exact, stub POSTs {N}`）、`B5-CHECK t11-fail-closed: PASS`（t11-drill:213 `drill: t11-pending: PASS`；两轮 3/3 pending、0 terminal、SUM(attempts) 3→6、`last_error LIKE '%audit connector HTTP transport failed%'` 全行）。手工复跑（本 spec 已执行，exit 0）：`DATABASE_URL=<throwaway> cargo run -p aero-audit-connector --bin aero-audit-{t11,relay}-drill`。
- **Oracle B（db_tests 翻转，实跑已证）**：`audit_governance::` 过滤器 6 测试全绿（E11）+ `moderation_finalize_outbox_parity` 独立槽全绿——`run_migrated_integration` 空过滤守卫（`test result: ok. [1-9]…`）防 vacuous green；B5 日志 `B5-CHECK audit_governance::: PASS` 与 `B5-CHECK moderation_finalize_outbox_parity: PASS`（不再 `SKIP (0239 not landed)`）。
- **Oracle C（pin guard）**：`bash scripts/test-b5-pin-guard.sh` → PASS（恰 37 槽：15 执行 + 22 `[PROPOSED]`；无重复/无 malformed/非空转）；test-integration.sh 末尾 `B5 contract pin: 37/37 … PASS` 且每个执行槽有 verdict 行。

### AC3 — 「a message create/edit/delete and an admin.moderation.action each produce exactly one audit_events row AND one outbox row status=0 with class/priority matching aero_ai::governance in the same PG tx (ROLLBACK removes both)」

> **证据驱动的范围钉化**：该验收的**可满足子集 = mapped token（`message.moderated` → admin 车道）**——`governance_lane_for` 对 message.create/edit/delete 返回 `None`（E7），0239 触发器只映射 `message.moderated`（E8）；`message.deleted` 进 admin 车道被 R-D2 负钉禁止（`user_delete_token_stays_out_of_admin_lane`）。**message.*/room.*/admin.* 全量 producer 属 sibling write-half direction（§3 红线）**——本 direction 保留该验收的 mapped 部分并钉死 pass-through 负侧。

- **Oracle A（mapped 子集同事务 + 列值，实跑已证）**：`moderation_finalize_outbox_parity`（audit_governance.rs:247，实跑 PASS）：BLOCK verdict 走生产 seam（`AiWorker::handle_moderate` → `soft_delete_outboxed_system`）→ 恰 1 条 `audit_events` 行（action='message.moderated'）+ 恰 1 条 outbox 行：`status=0`、`event_id=audit_events.id`（1:1）、`class='admin'`、`priority=100`、payload `action='admin.content.flag'`、`source_system` 匹配 binding。
- **Oracle B（ROLLBACK removes both，实跑已证）**：同测试 rollback half：binding 缺失 ⇒ 事务中止 ⇒ 消息未软删、blocks 未清、`orphaned==0`（无 audit 行逃逸）、outbox 行数不变（0 逃逸）。
- **Oracle C（pass-through 负侧，实跑已证）**：`non_moderation_action_passes_through_unmapped`（:628）：`room.create` 等非 moderation 行提交 → 0 条 v2 outbox 行、事务不因本触发器中止；单测面 `unknown_local_token_passes_through_unmapped`（room.create/message.create/message.edit/message.deleted/call.join/"" 全部 `None`）。
- **Oracle D（幂等）**：`duplicate_event_id_is_deduped_by_on_conflict`（:693）：同 event_id 二次插入被 `ON CONFLICT DO NOTHING` 吞掉，行数不变。

### AC4 — 「moderation finalize stamps class='admin' + action admin.content.flag per the mapping」

- **Oracle A**：AC3 Oracle A 的字段断言（class='admin' / priority=100 / payload action='admin.content.flag'）；单测面 `outbound_action_is_single_contract_token`（governance.rs：lane.outbound_action == MODERATION_OUTBOUND_ACTION == "admin.content.flag"、class == 'admin'、status == 0）+ `mapping_is_token_keyed`（同 token ⇒ 同 tuple，与调用者无关）。
- **Oracle B（v1 共存）**：`moderation_finalize_outbox_parity` half 4：`snaplink_delivery_outbox` 中 `destination='audit' AND idempotency_key=<audit id>` 恰 1 行且 payload action 保留**本地 token** `message.moderated`（v1 不被改写，E8）。

### AC5 — 「[PROPOSED] L1 gate: bulk-send path completes with zero network I/O inside the tx」

- **状态**：**[PROPOSED]，零代码存在**（grep 无任何 L1 聚合实现；`outbox.rs` 无 enqueue 方法，E6）——本 direction 不交付。
- **Oracle A（约束钉，已有）**：`admin_class_rows_never_aggregated`（governance.rs 单测）：admin 类行必须保持 1:1（`event_id` = `audit_events.id`），**永不**被高容量 `message.*` 窗口合并——L1 落地时本钉即其 bypass 分类权威。
- **Oracle B（约束声明）**：设计约束写死——L1 聚合**不得**在消息事务内发起 HTTP 调用、**不得**阻塞消息事务于 sink（方向原文："must never add an HTTP hop or block the message tx on the sink"）；落地归 sibling/L1 方向（§3）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| drill 翻转（AC1/AC2 Oracle A） | `crates/aero-audit-connector/src/bin/{aero-audit-t11-drill,aero-audit-relay-drill,aero-audit-priority-drill}.rs`（to_regclass/列探测 + 断言；t11 两轮 pending + SUM(attempts) + transport-error last_error；relay COUNT(status=2)==N + event_id parity；priority 首批 moderation + 全 drain + parity） | integration：throwaway 库 + 全链迁移 + stub sink；`scripts/test-integration.sh:325-461`（0239 文件门控解锁）+ 手工 `DATABASE_URL=… cargo run -p aero-audit-connector --bin …` |
| 403/422 dead 语义（AC2 分解面） | `crates/aero-audit-connector/tests/state_machine.rs`（`forbidden_dead_on_first_attempt` / `permanent_error_dead_after_exactly_two_attempts`）+ `src/relay.rs`（`deliver_claim` :173-231 / `is_dead_at` :48-49） | `cargo test -p aero-audit-connector`（非 ignored，无 DB；实跑 6/6 绿） |
| 同事务 parity + 列值 + pass-through（AC3/AC4） | `crates/aero-storage/src/audit_governance.rs` db_tests（6 项，实跑全绿） | `run_migrated_integration`（`--test-threads=1` + 空过滤守卫），`DATABASE_URL` + 已迁移 |
| 映射权威单测（AC4/AC5） | `crates/aero-ai/src/governance.rs` tests（6 项） | `cargo test -p aero-ai governance::`（无 DB；实跑 6/6 绿） |
| Q3 四桶 + dead 优先 | `crates/aero-eng/src/audit_provision.rs`（G0239_CANDIDATES :26 / Q3_SQL :48 / verdict :109-112）+ `crates/aero-eng/tests/audit_provision.rs` | `cargo test -p aero-eng audit_provision`（无 DB）+ `aero-cli audit-provision-check`（throwaway 迁移库）+ test-integration.sh provision legs |
| 提交门禁（R5） | governance.rs/0239/connector/audit_governance/audit_provision/drills/scripts 入库 | review + `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 |

## 7. Risks / [PROPOSED]

- **全 batch 产物 untracked（最高风险）**：0239/0240/0241、`crates/aero-audit-connector/`、`crates/aero-ai/src/governance.rs`、`crates/aero-storage/src/audit_governance.rs`、`crates/aero-eng/src/audit_provision.rs`(+tests)、drills、b5-pin/test-integration 改动均未提交——任何一次 `git reset --hard master`（AGENTS.md §4.1 多 agent 并行流程）都会抹掉本 direction 的全部验收对象。**提交是 AC1-AC5 全部 oracle 成立的前提**。
- **AC3 的字面表述超出本 direction 的映射契约**：direction 验收写 "message create/edit/delete … produce exactly one outbox row"，但 `governance_lane_for` 对 message.* 返回 `None`（R-D2 钉死 pass-through）。**spec 以 mapped 子集 + 负侧 oracle 落实**（§5 AC3），全量 producer 归 sibling write-half——若验收评审要求字面全量，那是 sibling 方向的交付，不是本 direction 的缺口。
- **「37-tests」为仓外契约标签**：本 spec 以 in-repo 可执行读法（b5-pin.sh 37 槽 + 具体测试名）落实；契约原文若冲突，改的是标签映射，不是测试。
- **迁移编译期嵌入**：漏 `cargo build` 直接 `aero-cli migrate` → 0239 静默 no-op、drill 仍 SKIP——AC1 Oracle A 的顺序（build → migrate）与反向断言（to_regclass 非 NULL）防此坑。
- **L1 聚合 [PROPOSED]**：方向原文要求 "never add an HTTP hop or block the message tx on the sink"——已作为 AC5 设计约束钉入；落地时必须以 `is_admin_class` 为分类权威、admin 行永不聚合（`admin_class_rows_never_aggregated` 钉）。
- **B5-3/B5-4 交接**：priority drill 的 PASS 现依赖工作树已落地的 `ORDER BY priority DESC` + 0240 索引（E3/E10）——语义所有权归 B5-3 方向；provision gate 的 boot 强制归 B5-4（本 direction 只保证其 Q3 可观测，`outbox-0239: table=… pending=… claimed=… delivered=… dead=…` 四桶成行）。

## 8. Sequencing / verification

1. **提交 batch 产物**（前置，§7 最高风险）：0239（+0240/0241 同批）+ governance.rs + connector + audit_governance.rs + audit_provision(+tests) + drills + `scripts/` + `Cargo.toml` 改动。
2. **快速 oracle**：`cargo test -p aero-ai governance::`（6/6）→ `cargo test -p aero-audit-connector`（非 ignored 全绿）→ `cargo test -p aero-eng audit_provision`。
3. **全链 integration**：`cargo build` → `bash scripts/test-integration.sh`——`audit_governance::` / `moderation_finalize_outbox_parity` / `a3-relay-drill` / `t11-fail-closed` / `audit-provision-check` 槽全 PASS；`moderation-priority-drill` 槽 PASS（B5-3 fold-in 已落地）或 SKIP-with-reason；pin guard `37/37 PASS`。
4. **Q3 手工面**：throwaway 迁移库上 `aero-cli audit-provision-check` → `outbox-0239: table=audit_governance_outbox pending=… claimed=… delivered=… dead=…` 四桶；dead>0 时 verdict fail-closed（"dead is never counted delivered"）。
5. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规。
6. **sibling 交接**：message.*/room.*/admin.* 全量 in-tx producer（write-half, `crates/aero-im-core`）与 L1 聚合（[PROPOSED]）不在本 direction；mapped 子集契约（governance.rs + 0239 触发器）即其消费面。
