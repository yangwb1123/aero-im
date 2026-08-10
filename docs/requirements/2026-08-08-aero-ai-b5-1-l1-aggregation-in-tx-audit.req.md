# Requirements Spec — aero-ai B5-1 item 1 completion：message.create/edit in-tx 审计 + L1 聚合 outbox 行（admin-class 1:1 钉死）

- **Module (analysis root)**: `crates/aero-ai/src`（`governance.rs` 为分类权威 + 验收 oracles；L1 触发器落 `migrations/0242_*.sql`，仓储测试落 `aero-storage/src/audit_governance.rs`，drill 腿扩展落 `aero-audit-connector`——与 0239 slice 同构的跨 crate 落地）
- **Direction**: "L1 aggregation + in-tx audit for high-volume message.create/edit backlog (B5-1 item 1 completion)"（value 9 / risk_reduction 7 / effort 8 / confidence 6）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json` (direction #1)
- **Campaign**: `aero-im-b5-outbox-relay`；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md:8`（B5-1：0239 DDL、P2 parity event_id 1:1、‡ 类走 L1）；gate anchor `docs/campaigns/implementation-gate.md:64`（"Outbox + in-tx：DDL（status 0/1/2/3 normative）；`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1"）+ :78（G6：37/37、T-11、moderation 优先级）
- **Status**: Requirements（下述证据全部经源码 grep 核对；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-ai/src/governance.rs:86-101`（`is_admin_class`，"[PROPOSED] L1 aggregation bypass"） | ✅ **Verified**（行号微漂）。`governance_lane_for` :74-83：唯一映射 = `message.moderated` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`；**其余 token 全部 `None` pass-through**（含 `message.create`/`message.edit`/`room.create`/`message.deleted`/`call.join`/`""`）。`is_admin_class` :90-96，doc 注释 :86-89 明言 "R5 classification for the [PROPOSED] L1 aggregation bypass: admin-class rows must stay 1:1 (`event_id` = `audit_events.id`), never merged by the high-volume `message.*` window"。单测 `admin_class_rows_never_aggregated` :195-199（citation :191-199 微漂）：`is_admin_class(LOCAL_ACTION_MODERATED)` 真、`message.create`/`room.create` 非 admin-class。文件共 6 项单测。`crates/aero-ai/src/lib.rs:37` re-export `governance_lane_for, is_admin_class, GovernanceLane, GOVERNANCE_CLASS_ADMIN, GOVERNANCE_PRIORITY_MODERATION, MODERATION_OUTBOUND_ACTION`。**全仓 grep：`is_admin_class`/`governance_lane_for` 除 re-export 外零消费者**——分类权威已就位、bypass 从未建（direction 陈述准确）。 |
| E2 | `crates/aero-storage/src/message/crud.rs:31`（`MessageRepo::insert`，无 audit 写） | ✅ **Verified（精确）**。`insert` :31-40：begin → `lock_reply_parent_in_tx` → `insert_row_in_tx` → commit；**无任何 audit 写**。⚠️ **钉化**：`insert` 的**全部调用者都是 `#[cfg(test)]` 夹具**（notification.rs:467+、block_interaction.rs:223、message_reports.rs:401、audit.rs:695、search.rs:448、crud.rs:619、thread_title.rs:102+、audit_governance.rs:98）；**生产 create 路径 = `insert_outboxed`**（`crates/aero-storage/src/message/idempotency.rs:158`，被 `aero-im-core/src/service/messages.rs:216` 的 send 路径调用）——审计写入点定在生产路径（§7 D4）。`insert_outboxed` :158-240：单事务含 room-access 锁、reply-parent 锁、attachment 锁、`insert_row_in_tx`、event_outbox 行、side-effect 行、send-key 幂等 claim、commit；client_message_id 重放早退（:167-173）不产生新行。 |
| E3 | `crud.rs:359-407`（delete 路径带 `AuditRepo::append_in_tx`） | ✅ **Verified（精确）**。`soft_delete_audited` :364-384（`message.deleted`，同事务：软删成功才 append，append 失败整体回滚）；`soft_delete_moderated` :398-417（`message.moderated`，actor=None）。另 `events.rs:267` `soft_delete_outboxed_system`（worker/mod.rs:372-404 `handle_moderate` 与 messages.rs:611 `moderate_delete` 的落点）→ `soft_delete_locked_outboxed_in_tx` 内 append :337（workspace/action 均为 Option，`None` 不写）；`authorization.rs:368`（`message.recalled`）。**生产 edit 路径同样无 audit**：`edit_outboxed_authorized`（authorization.rs:403-440，单事务）→ `edit_locked_outboxed_in_tx`（events.rs:69-160：message_edits 历史行 + `RoomEvent::Edited` outbox，**无 audit append**）；crud.rs:168 的裸 `edit` 是 pool 级旧路径（调用者仅测试）。 |
| E4 | `crates/aero-common/src/model/audit.rs`（`GOVERNANCE_CLASS_MESSAGE`/`GOVERNANCE_CLASS_ROOM` vocabulary） | ✅ **Verified（精确）**。`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` :150、`LOCAL_ACTION_MODERATED = "message.moderated"` :156、`GOVERNANCE_CLASS_ADMIN/MESSAGE/ROOM` :164/:166/:168（`AuditClass` 派生）；字面量 pin 测试 :269-276。**message 类尚无出站 token 常量**——L1 聚合信封的 `action` 是新叶常量（§4 R4）。 |
| E5 | `migrations/0239_audit_governance_outbox.sql`（1:1 `event_id` PK + status CHECK 0..3） | ✅ **Verified**。DDL：`event_id UUID PRIMARY KEY`（= `audit_events.id` 1:1）、`status INTEGER NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))`、`class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`、`priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`、`delivery_mode DEFAULT 'push' CHECK`、`payload JSONB NOT NULL CHECK (jsonb_typeof='object')`、attempts/claim_token/lease_expires_at/delivered_at/last_error + claim-state CHECK + due 部分索引。触发器 `aero_enqueue_governance_audit()`：**仅 `NEW.action = 'message.moderated'`**（token-keyed）→ 1:1 入 outbox（class 'admin'/priority 100/status 0/0239 信封含 `idempotency_key = NEW.id::text`），`ON CONFLICT (event_id) DO NOTHING`；其余 `RETURN NEW` pass-through 不 raise；Gate 1 runtime-enabled（fail-open）、Gate 2 binding 缺失 RAISE（fail-closed，仅 moderation 行）。**L1 聚合行必须与 0239 行形态可调和**（同一表：PK 约束 + status/class/priority/payload CHECK——聚合行的 `event_id` 不能是 `audit_events.id`（合成窗口键），P2 parity 只适用于非聚合行，contract 的 "‡ 类走 L1" 即此豁免）。 |
| E6 | `crates/aero-storage/src/audit_governance.rs`（DDL cross-pin drills） | ✅ **Verified**。933 行；5 项 db_tests：`moderation_finalize_outbox_parity` :247（**断言 gov 恰 1 行** + event_id 1:1 + rollback/replay 半部）、`ddl_contract_defaults_and_checks` :425、`moderation_finalize_runtime_disabled_commits_1_plus_0` :583、`non_moderation_action_passes_through_unmapped` :628（token = `message.deleted`，断言 gov 0 行 + v1 行仍流）、`duplicate_event_id_is_deduped_by_on_conflict` :693。夹具 `message_in_workspace` :80-98 用 **raw `MessageRepo::insert`**（E2）——本切片把审计写入点放在生产路径（`insert_outboxed`/`edit_outboxed_authorized`），**该夹具不产审计行 ⇒ parity 测试的恰 1 行断言不被破坏**（§7 风险③）。helper：`fixture` :44、`enable_enforcement_with_binding` :121、`restore_enforcement_disabled` :170、`moderate_finalize` :184、`count_governance_rows` :203、`reset_governance_table` :232。 |
| E7 | `crates/aero-storage/src/audit.rs`（`AuditRepo::append_in_tx`） | ✅ **Verified（精确）**。`append_in_tx(tx, workspace, actor, action, target, detail) -> Result<AuditId>` :127-136 → `append_on` :140-172：事务内 `INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)`，返回新 `AuditId`（ULID）。`audit_events` DDL = `migrations/0007_audit.sql`（**workspace_id NOT NULL REFERENCES workspaces**——未知 workspace 注入即 FK 错误，events.rs:614 测试的失败机制）；每日 RANGE 分区 `migrations/0146`（`ensure_audit_event_partitions` 预建，INSERT 失败窗口极小）；retention 清扫 `sweep_before` :64（legal-hold 守卫）。 |
| E8 | `events.rs:614`（`audit_failure_rolls_back_delete_and_outbox_append`） | ✅ **Verified（精确）**。`#[ignore]` db_test：`soft_delete_outboxed_system(id, Some(WorkspaceId::new()), …)`（未知 workspace → FK 失败）→ 断言消息保留（`deleted_at IS NULL`）+ event_outbox 无 delete 事件——**本切片 AC1 的 rollback 半部镜像此机制**。 |
| E9 | v1 0236 路径（`snaplink_delivery_outbox`，无聚合） | ✅ **Verified**。`audit_events_snaplink_delivery` AFTER INSERT 触发器（0236:129-133）把**每条** audit 行 1:1 入 v1 outbox（runtime-enabled 门；binding RAISE）。direction 陈述 "High-volume rows … flow through the v1 0236 path without aggregation" 准确。v1/v2 共存是既定 cutover 语义（parity 测试 Half 4 钉），本切片不碰 v1。 |
| E10 | connector 对行形态的兼容面 | ✅ **Verified（零改动前提）**。`pg.rs:69-108` `claim_due`：`status IN (0,1)` + `available_at`/lease 过滤 + `ORDER BY (available_at, created_at, event_id)` + `FOR UPDATE SKIP LOCKED` + `LIMIT [1,500]`——**对 payload/event_id 形态无任何假设**；:68 `event_id: AuditId::from_uuid(row.event_id)`（任意 128-bit UUID 可转，md5 派生亦合法）。`client.rs:145` `Idempotency-Key` = `claim.event_id` Display（ULID base32，outbox.rs:33-35）；receipt echo 校验 `receipt_event_id_matches` :393-400 **value-level 双格式**（UUID text 或 ULID base32 均可）——聚合行 event_id（非 ULID 形态）照常 settle。`relay.rs`：403 → 首次即 `mark_dead`（:199-213，T-11 fail-closed）；422/409 类 permanent → ≤1 retry → dead；transient → requeue（:220-238）。 |
| E11 | 0241 reconciler 隔离性 | ✅ **Verified**。`aero_enqueue_governance_reconcile` 类扫描 **token-keyed 到 `message.moderated` 仅**（"backfilling an unmapped action would FABRICATE a governance claim"）——不会扫到 message.create/edit；delivered = durable cursor、dead 永不复活。**L1 聚合走无条件触发器 ⇒ 无 disabled-window 缺口 ⇒ 不需要 message 类 reconciler**（§7 D2）。 |
| E12 | harness 钉 | ✅ **Verified**。`scripts/b5-pin.sh:37-38` 命名条目 `audit_governance::` + `moderation_finalize_outbox_parity`（空过滤守卫防 vacuous green）；`scripts/test-integration.sh` B5 段：`run_migrated_integration` 两条（:306-318，`-p aero-storage --lib`）+ A3 relay drill（:325-341）+ T-11 drill（:347+），全部以 `migrations/0239_audit_governance_outbox.sql` 文件存在为门（0239 已存在 → 已 un-gated）。drill bin 探测 `to_regclass('audit_governance_outbox')` 缺失 exit 2 SKIP（relay-drill.rs:57-67 / t11-drill.rs）。 |
| E13 | migrations 序号 | ✅ **Verified**。`ls migrations/*.sql | wc -l` = **241**，尾件 = 0241 → **下一序号 0242**。`git status --short migrations/`：0239/0240/0241 全部 **untracked**（连同 governance.rs、audit_governance.rs、model/audit.rs、aero-audit-connector/）——上一批次未提交（§7 风险①）。 |
| E14 | （补充）gate anchor | ✅ **Verified**。`docs/campaigns/implementation-gate.md` aero-im 行 1 = "Outbox + in-tx：DDL（status 0/1/2/3 normative）；`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1"，验收 = "30 个忽略测试 CI 全绿（37/37）；P2 parity"。sibling auth slice（`2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.req.md`）已落地 `AuditGovernanceOutboxRepo` 的 **admin 域**显式写路（R6：无条件入队、不 consult runtime 门）——本切片 message 域的 L1 形态与其正交、共用同一张 outbox 表。 |

### 1.1 对 direction problem 陈述的勘误/钉化（evidence-backed）

- **行号微漂（不改变结论）**：`is_admin_class` 实际 :90-96（:86-89 是含 "[PROPOSED]" 字样的 doc 注释）；`admin_class_rows_never_aggregated` 实际 :195-199。引用 :86-101/:191-199 属核对时锚点，按 AGENTS.md §0 以符号为准。
- **「`MessageRepo::insert` 无 audit」准确，但生产 create 路径是 `insert_outboxed`**（E2）：raw `insert` 的全部调用者均为测试夹具。审计写入点 = `insert_outboxed`（send 路径）与 `edit_outboxed_authorized`（edit 路径）——direction 的验收（"message insert + audit row + aggregate outbox row commit together"）由生产路径满足；raw `insert` 是否跟进是 D4 决策点（默认 deferred，避免 ~30 夹具 churn 与 parity 测试恰 1 行断言）。
- **「create/edit 既不在-tx 审计也不可聚合」成立且双侧确认**：create 侧 `insert_outboxed` 零 audit（E2）；edit 侧 `edit_locked_outboxed_in_tx` 只写 `message_edits` 历史 + event outbox，零 audit（E3）。message_edits 是内容历史（版本捕获），审计行是独立记录——两者都保留。
- **L1 聚合的机制钉为「AFTER INSERT ON audit_events 并行触发器」**：与 0239 触发器同构（审计行 = 触发输入 ⇒ in-tx 原子性自动成立）；**不改 0236/0239 触发器与 0239 DDL 一字**（R5 纪律，sibling auth slice 同款）；admin-class 1:1 由「触发器 token allowlist 排除 message.moderated」这一**运行时守卫** + 既有单测 + 新行为 drill 三重钉死（AC3）。
- **acceptance 的 "audit failure rolls back the insert" 语义 = message create/edit 的审计写是 fail-closed**（镜像 events.rs:614 的 delete 语义）：audit_events INSERT 失败 ⇒ 整事务回滚 ⇒ 消息不发。这是 direction 明示的验收（非 fail-open）。运行期影响：audit 分区维护（0146 预建）是既有缓解（§7 风险②）。
- **connector 零改动成立**（E10）：聚合行是普通 outbox 行——claim/settle/403→dead 全部按既有状态机；`Idempotency-Key` 携带的是 `AuditId` Display 编码的聚合 event_id（不是 UUID text），receipt echo value-level 双格式可对。drill 只需**扩展种子形态**，不需改断言语义。

## 2. Verified current state

```
分类权威（已在，untracked ⚠️）     crates/aero-ai/src/governance.rs（governance_lane_for :74-83 / is_admin_class :90-96
                                  / admin_class_rows_never_aggregated :195-199 + 6 单测；[PROPOSED] bypass 从未建）
生产 create 路径（无审计）          idempotency.rs:158 insert_outboxed（im-core messages.rs:216 调用；单事务 + 幂等早退）
生产 edit 路径（无审计）            authorization.rs:403 edit_outboxed_authorized → events.rs:69 edit_locked_outboxed_in_tx
                                  （message_edits 历史 + Edited outbox，无 audit append）
带审计的既有路径（先例）            crud.rs:364 soft_delete_audited · :398 soft_delete_moderated · events.rs:267,337
                                  soft_delete_outboxed_system · authorization.rs:368 message.recalled
audit 写入仓储（已在）              audit.rs:127 append_in_tx → audit_events（0007 DDL，workspace_id NOT NULL FK；
                                  0146 每日分区 + 预建；retention sweep :64）
v2 outbox（已在，untracked）        0239 DDL + token-keyed 触发器（仅 message.moderated 1:1 入）；0240 due 索引；0241 reconciler
                                  （token-keyed 隔离 message 类）；audit_governance.rs 5 项 db_tests + 全套夹具
connector（已在，untracked）        claim_due/settle/requeue/mark_dead + 403→dead（T-11）+ Idempotency-Key=AuditId Display
                                  + receipt value-level 双格式；3 个 drill bin（to_regclass 探测 SKIP）
v1 路径（已在）                    0236 audit_events_snaplink_delivery（每 audit 行 1:1 入 snaplink_delivery_outbox，无聚合）
harness（已在）                    b5-pin.sh:37-38 命名钉 + test-integration.sh B5 段（0239 文件门已 un-gated）
叶 vocabulary（已在）              model/audit.rs（MODERATION_OUTBOUND_ACTION/GOVERNANCE_CLASS_*；无 message 类出站 token）

缺的唯一一环（本 direction 关闭）：
  A) message.create/edit 的 audit 行（生产路径 in-tx 写）—— 不存在
  B) L1 聚合 outbox 行（每 (workspace, window) 一条带 count 的行）—— 不存在（[PROPOSED] bypass 从未建）
  C) admin-class 1:1 的运行时守卫（触发器 allowlist + 行为 drill）—— 不存在（仅单测钉）
```

## 3. Scope

**In scope（B5-1 item 1 completion，effort 8 的完整范围）**：
- aero-storage（生产写点）：
  - `insert_outboxed`（idempotency.rs:158）：同事务追加 `message.create` audit 行（workspace 从 rooms 表 in-tx 解析——`rooms.workspace_id NOT NULL`；actor = sender；target = message id）；共享 `pub(crate)` helper 供测试注入失败（镜像 events.rs:614 机制）。
  - `edit_outboxed_authorized`（authorization.rs:403）：同事务追加 `message.edit` audit 行（actor = editor；target = message id；detail 含 version）。
- migrations/0242：`aero_enqueue_l1_aggregate_audit()` AFTER INSERT ON audit_events 触发器——token allowlist（`message.create`/`message.edit`）→ 每 (workspace, class, 60s window) 一条聚合行（确定性 `event_id`、payload 带 `count`）；admin-class（`message.moderated`）与其余 token 一律 pass-through（运行时守卫）。
- aero-common + aero-ai：message 类出站 action 叶常量（`MESSAGE_OUTBOUND_ACTION`，名称 [PROPOSED]）+ governance.rs re-export + pin 测试 + `[PROPOSED]` 注释更新为 landed 引用。
- aero-storage db_tests（AC1/AC2/AC3）+ aero-audit-connector drill 腿扩展（AC4，混合行形态种子）。
- 文档钉：0239/0240/0241 DDL 与触发器**逐字保持**；`governance.rs` 的 `[PROPOSED] L1 aggregation bypass` 注释改为指向 0242 触发器。

**Out of scope（不扩）**：
- **不改** 0239/0240/0241 DDL 与触发器、不改 `governance_lane_for` 映射（admin 类 1:1 语义与 token-keyed 映射逐字保持）。
- **不做** room.* 类聚合（低容量，保持 pass-through；contract 的 room.* in-tx 写入属其它切片）、不改 v1 `snaplink_delivery_outbox` 路径（v1/v2 共存既定）、不动 0241 reconciler。
- **不做** 用户侧 delete/recall 路径的审计补写（`soft_delete_outboxed_authorized` 的 workspace 解析是独立缺口，属既有审计覆盖范围问题，非本 direction 验收面——direction 只点名 create/edit）。
- **不做** connector 状态机改动（零改动是验收前提，E10）。
- **不做** raw `MessageRepo::insert`/`edit`（crud.rs）的审计化（D4：全部调用者为测试夹具，deferred，仅文档注记）。
- 不新增 outbox 表/列/索引（聚合行复用 0239 表，due 索引兼容）。

## 4. Requirements

**R1（message.create in-tx 审计，fail-closed）**：生产 create 路径 `insert_outboxed`（idempotency.rs:158）在既有单事务内、`insert_row_in_tx` 之后、commit 之前追加恰 1 行 `audit_events`：`action = 'message.create'`、`actor_id = sender`、`target = message id`、`workspace_id` = 同事务内从 `rooms.workspace_id` 解析、`detail` = JSON 对象（含 `room_id`、`message_id`、`block_count`）。写入经共享 helper（签名形如 `append_message_create_audit_in_tx(tx, workspace, message, sender, detail) -> Result<AuditId>`，`pub(crate)`——测试可注入未知 workspace 触发 FK 失败）。**任一失败（含 audit 失败）→ 整事务回滚，消息行不存在**（镜像 events.rs:614 的 fail-closed 语义；"no message row without its audit row"）。幂等重放（同 `client_message_id` 早退路径，:167-173）不产生新 audit 行——重试不双记。

**R2（message.edit in-tx 审计，fail-closed）**：生产 edit 路径 `edit_outboxed_authorized`（authorization.rs:403）在既有单事务内追加恰 1 行 `audit_events`：`action = 'message.edit'`、`actor_id = editor`、`target = message id`、`workspace_id` 同事务解析（`resolve_message_target` 已有 room 解析，补 workspace 查询）、`detail` 含 `version`。失败 → 版本不推进、无 `Edited` 事件、无审计行（整事务回滚）。`message_edits` 内容历史与 `RoomEvent::Edited` outbox 保留不动（审计行是独立记录）。

**R3（L1 聚合触发器，新迁移 `migrations/0242_audit_governance_l1_aggregate.sql`）**：`aero_enqueue_l1_aggregate_audit()` AFTER INSERT ON `audit_events` FOR EACH ROW（与 0236/0239 触发器并行共存，**不改两者**）：
- **Token allowlist（运行时守卫）**：`NEW.action IN ('message.create','message.edit')` 才聚合；其余（含 `message.moderated`、`message.deleted`、`room.*`、`auth.*`）`RETURN NEW` pass-through 不 raise、不建行。
- **窗口**：固定 60s；`window_start = to_timestamp(floor(extract(epoch FROM NEW.created_at) / 60) * 60)`（deterministic，drill 可重算；§7 D1）。
- **确定性 event_id（PK）**：`md5(workspace_id::text || '|' || class || '|' || to_char(window_start, …))::uuid`——每 (workspace, class, window) 唯一 ⇒ `ON CONFLICT` upsert 成立（并发插入经行锁串行化，count 原子收敛）。
- **首事件建行**：`INSERT (event_id, status 0, class 'message', priority 10, payload)`，payload 带 `count = 1`；**后续事件**：`ON CONFLICT (event_id) DO UPDATE SET payload = payload || jsonb_build_object('count', (payload->>'count')::int + 1, 'last_event_at', NEW.created_at) WHERE audit_governance_outbox.status = 0`；`ROW_COUNT = 0`（窗口行已被 claim/delivered/dead）→ **spill 行**（fresh `gen_random_uuid()` event_id、`count = 1`、payload 带 `spill = true`）——**事件永不丢失、永不改写 in-flight/终态行**（§7 D5）。
- **无条件入队（无 runtime 门、不 consult binding）**：与 sibling auth slice R6 一致——审计投递不因 `snaplink_commercial_runtime.enabled` 被抑制；message 类因此无 disabled-window 缺口，**不需要 0241 式 reconciler**（E11；§7 D2）。0235 的 metering 触发器（messages INSERT 侧）语义不受影响（本触发器只挂 audit_events）。
- **信封**：镜像 0239 形状但无 binding 派生字段：`event_id`、`event_type 'aero.im.security'`、`schema_id 'aero.im.security'`、`schema_version 1`、`occurred_at = window_start`、`window_start`、`window_end`、`class 'message'`、`aggregate_type 'workspace'`、`aggregate_id = workspace_id::text`、`action = MESSAGE_OUTBOUND_ACTION`、`count`、`first_event_at`、`last_event_at`、`idempotency_key = event_id::text`、`data_classification 'confidential'`、`retention_class 'security'`。payload 恒为 object（0239 CHECK 满足）。

**R4（message 类出站 token 叶常量）**：`aero-common/src/model/audit.rs` 新增 `pub const MESSAGE_OUTBOUND_ACTION: &str = "message.activity"`（**名称 [PROPOSED]**，单一来源，镜像 `MODERATION_OUTBOUND_ACTION` :150）+ 字面量 pin 测试（:269-276 区段追加）；`governance.rs` re-export（镜像 MODERATION_OUTBOUND_ACTION 链）；0242 触发器用字面量 + 文本 cross-pin 注释（aero-storage 不得 import aero-ai，既有纪律）。

**R5（分类权威与运行时 1:1 钉，不动映射）**：`governance_lane_for`/`is_admin_class` **逐字保持**（0239 触发器、sibling auth 显式写路均依赖）；`admin_class_rows_never_aggregated` 等 6 项单测保持；**新增运行时守卫 = R3 触发器 allowlist**（token-keyed，与 `governance_lane_for` 的映射形状一致：admin 类绝不被聚合窗合并）。`governance.rs` :86-89 的 `[PROPOSED] L1 aggregation bypass` 注释更新为指向 0242 触发器的 landed 引用。

**R6（connector 零改动兼容）**：聚合行是普通 outbox 行：`claim_due`（status IN (0,1)、due 索引）照常认领；`Idempotency-Key` = `AuditId::from_uuid(event_id)` 的 Display（ULID base32，任意 128-bit 可编码）；receipt echo value-level 双格式校验（client.rs:393-400）对 md5 派生 event_id 成立；403 → 首次即 dead、422/409 类 ≤1 retry → dead、transient → requeue——**两种行形态行为一致，connector 代码零改动**（drill 仅扩种子）。

**R7（既有套件不回归）**：`audit_governance.rs` 既有 5 项 db_tests 逐字保持（名字被 harness 钉死：b5-pin.sh:37-38）；`moderation_finalize_outbox_parity` 的 "gov 恰 1 行" 断言成立（夹具走 raw `insert` 不产审计行；`message.moderated` 被 allowlist 排除）；`non_moderation_action_passes_through_unmapped` 的 token `message.deleted` 不在 allowlist（gov 0 行断言保持）；0241 reconciler 不动（token-keyed 隔离，E11）；0236 v1 行继续 1:1 流（本切片不碰）。

**R8（retention/清扫语义注记，非新要求）**：`audit_events` 行按 retention 清扫（audit.rs:64 sweep_before + 0146 分区 drop，legal-hold 守卫）；**聚合行是紧凑持久记录**，不受 audit retention 约束（delivered 行 = durable cursor，0241 注释语义）——这正是聚合的价值（窗口压缩后仍可投递）。信封只带时间戳、**不带 first/last event_id 引用**（避免清扫后悬空引用）。

## 5. Acceptance checks（direction 原样保留，逐条 testable）

> direction acceptance 四条全部保留；AC1-AC3 为 PG 门控 db_tests（`#[ignore = "requires live Postgres"]` + `DATABASE_URL` 门控，复用 `audit_governance.rs` 既有夹具），AC4 为 drill 腿扩展，AC5 为全仓门禁。

- **AC1（in-tx rollback，镜像 events.rs:614）**：db_test。
  - Commit 半部（create）：`insert_outboxed` 提交后——`audit_events` 恰 1 行（`action='message.create'`、`target` = message id、`actor` = sender、workspace = room 的 workspace）+ `audit_governance_outbox` 恰 1 行（`status=0`、`class='message'`、`priority=10`、`payload->>'count' = '1'`）——三组行同一事务（rollback 半部反证）。
  - Commit 半部（edit）：`edit_outboxed_authorized` 提交后——恰 1 行 `message.edit` audit + 同窗口聚合行 `count` 递增（= 2）。
  - **Rollback 半部（create，镜像 events.rs:614 的机制）**：开事务 → `insert_row_in_tx` 插入消息行 → 以 `WorkspaceId::new()`（无对应行）调 `append_message_create_audit_in_tx` → `expect_err`（FK 违例，`sqlx::Error::Database`）→ rollback → 断言：消息行不存在、零 audit 行、`count_governance_rows` 不变（**聚合 upsert 随事务回滚**，count 不虚增）。
  - Rollback 半部（edit）：同形——edit in-tx 注入未知 workspace → 版本不推进、无 audit 行、聚合 count 不变。
  - 幂等腿：同 `client_message_id` 重放 `insert_outboxed` → 不新增 audit 行、聚合 count 不变。
- **AC2（L1 drill）**：db_test。同 workspace + 同 room、一个 60s 窗口内 N=5 条生产路径消息 → `audit_governance_outbox` 恰 **1** 条聚合行：`event_id` = SQL 重算的确定性键（`md5(workspace||'|'||'message'||'|'||window_start)::uuid`，断言相等）、`payload->>'count' = '5'`、`payload->>'idempotency_key' = event_id::text`、`class='message'`、`status=0`、`priority=10`；**`COUNT(outbox) < COUNT(audit_events)`**（1 < 5）。第二个 workspace → 第二条聚合行（per (workspace, class)）。同窗口混入 `message.edit` → 同一行 `count=6`。跨窗口：直接 SQL 插入 `created_at` 落在上一分钟的 audit 行（触发器输入相同）→ 第二条聚合行（event_id 重算为不同窗口键），两窗并存。⚠️ 新 db_tests 以 `to_regprocedure('aero_enqueue_l1_aggregate_audit()')` 运行时探测门（0242 未落库 → SKIP 式早退，不红 0239 门控的 harness 条目）。
- **AC3（admin-class 1:1 运行时钉）**：既有 `admin_class_rows_never_aggregated`（governance.rs:195）保持绿；**新增** db_test——(a) 先有 N 条 create（聚合行 count=N）后 `moderate_finalize`：`message.moderated` 恰 1 条 admin 行（`event_id = audit_events.id`（1:1）、class 'admin'、priority 100、status 0）**且聚合行原封不动**（count 仍 N、无第二行、无增量）；(b) 直接 SQL 逐 token 插 audit 行（`message.moderated`/`message.deleted`/`room.create`/`auth.login`/`""`）→ 零聚合行被建/被增。
- **AC4（relay/T-11 drill 双形态仍绿）**：
  - A3 relay drill 腿扩展：种子混合形态——1:1 形（v4 uuid event_id）+ 聚合形（md5 派生 event_id、payload 带 count）→ 全部 `status=2`（既有 `COUNT(status=2)==N` + event_id 集合 parity 断言自然覆盖）；**Idempotency-Key = 聚合 event_id**（`AuditId` Display 编码）经 stub sink receipt echo + `validate_audit_receipt` value-level 校验证明——任何不匹配即 permanent → dead，status-2 parity 必红。
  - T-11 drill 腿扩展：混合形态种子 + relay 缺席（403）→ 两种形态**同样** `mark_dead`（status 3）、零 delivered、逐轮 attempts 证据、`last_error` transport——fail-closed 行为对两种行形态不变。
  - 门：drill 腿沿用 0239 文件门（connector 行为形态无关，0239 未落即 SKIP 的既有语义不变）；`test-integration.sh` 零新增条目。
- **AC5（全仓门禁）**：`cargo test --workspace --lib` 全绿（PG 门控测试 `-- --ignored` + 一次性 throwaway 库，`make migrate-smoke` 先验 fresh-deploy）；既有 governance.rs 6 单测 + audit_governance.rs 5 db_tests 逐字保持绿；`cargo clippy --workspace --all-targets` 无新增警告；`scripts/{truth-check,file-size-check,web-check}.sh` 0 违规；`scripts/test-integration.sh` B5 段（`audit_governance::`/`moderation_finalize_outbox_parity`/`a3-relay-drill`/`t11-fail-closed`/`moderation-priority-drill`）全 PASS。

## 6. Test placement

- **AC1/AC2/AC3 的 db_tests**：`crates/aero-storage/src/audit_governance.rs` 同模块（harness 过滤器 `audit_governance::` 已存在且 un-gated，E12——零 harness 改动；复用 `fixture`/`reset_governance_table`/`count_governance_rows`/`enable_enforcement_with_binding` 夹具）。AC1 的 create 腿亦可落 `message/idempotency.rs`（`insert_outboxed` 所在模块）；两处均在 `-p aero-storage --lib` 下。
- **AC2 的 0242 门**：新 db_tests 用 `to_regprocedure('aero_enqueue_l1_aggregate_audit()')` 探测早退（A3 drill 的 `to_regclass` 探测先例），保证 0242 落地前 `audit_governance::` 条目不红。
- **AC4 的 drill 腿**：`crates/aero-audit-connector/src/bin/aero-audit-relay-drill.rs` + `aero-audit-t11-drill.rs` 种子集扩展（混合形态），既有断言不动。
- **单测 pin**：`governance.rs` 既有 6 项（含 `admin_class_rows_never_aggregated`）+ `model/audit.rs` pin 测试（追加 `MESSAGE_OUTBOUND_ACTION` 字面量断言）。
- **AC5**：既有全仓门禁，零新增。

## 7. Risks / [PROPOSED] / 决策点

- **D1（窗口大小）**：固定 60s（触发器常量；deterministic，drill 可重算）。GUC 可调（`set_config('aero.l1_window_seconds', …)`）为后续选项，[PROPOSED]——本切片不引 GUC，避免 0242 与 drill 的配置面。
- **D2（runtime 门）**：**默认无条件入队**（选定）——镜像 sibling auth R6（审计不因商业开关被抑制）+ 无 disabled-window 缺口（0241 式 reconciler 对 message 类不需要）+ drill 无需 enable enforcement。备选（runtime 门）被拒：会引入 disabled-window 缺口 + reconciler 扩展 + drill 需 binding 装配，超出本 direction 验收面。
- **D3（`MESSAGE_OUTBOUND_ACTION` 命名）**：`'message.activity'` 为 [PROPOSED]；sink 契约在仓外，若钉不同名 → 单行叶常量改动 + pin 测试自动跟随（R4 的单一来源设计即是为此）。
- **D4（raw `MessageRepo::insert`/`edit`，crud.rs）**：默认 deferred——全部调用者为 `#[cfg(test)]` 夹具（E2/E3），审计化 = ~30 夹具 DB 状态变更 + `moderation_finalize_outbox_parity` 恰 1 行断言改版（若未来反转 D4，parity 测试 Half 3 需改为 "恰 1 admin + 1 aggregate" 并更新 doc）。crud.rs 的 `insert`/`edit` doc 注释补一句 "test/internal helper；生产路径 insert_outboxed / edit_outboxed_authorized 负责 in-tx 审计"。
- **D5（spill 语义）**：窗口聚合行被 claim/delivered/dead 后到达的事件 → 1:1 spill 行（fresh event_id、count=1、`spill=true`）。有界（仅窗口行首次被认领后的迟到事件）；永不丢事件、永不改写 in-flight/终态行（claim 中改写会破坏 settle 的 fenced 重读）。spill 行仍是普通 outbox 行，投递语义不变。
- **风险①（上一批次未提交）**：0239/0240/0241 + `governance.rs` + `audit_governance.rs` + `model/audit.rs` + `aero-audit-connector/` + harness 改动全部 untracked（E13）——本切片落地前**必须先提交**，否则 `git reset --hard` 校准会丢 B5-1 全部地基（sibling 文档同款义务）。
- **风险②（fail-closed 审计的可用性面）**：AC1 钉死 audit 失败回滚消息发送 ⇒ `audit_events` 写入可用性成为消息链路前置。缓解：0146 分区预建（INSERT 失败窗口极小）+ 既有 metering 触发器（0235）已有同款 fail-closed 先例。
- **风险③（parity 测试恰 1 行断言）**：本切片靠「审计写入点在生产路径、夹具走 raw insert」保持其绿（E6）。任何未来把 raw `insert` 审计化的改动必须同步改该测试（§7 D4 已记）。
- **风险④（并发 count 收敛）**：同窗口并发插入经 `ON CONFLICT DO UPDATE` + 行锁串行化，count 原子递增（先到者建行、后到者阻塞后 DO UPDATE）；`IF NOT FOUND`/`ROW_COUNT=0` 只发生在 status≠0（终态行），不产生丢计数。AC2 的 N=5 顺序 drill 不覆盖并发面——并发正确性由机制保证（PG 行锁语义），不新增并发测试（超出验收面）。

## 8. Sequencing

1. **前置**：提交上一批次 untracked 物（0239/0240/0241、`governance.rs`、`audit_governance.rs`、`model/audit.rs`、`aero-audit-connector/`、`test-integration.sh`/`b5-pin.sh` 改动、docs）——B5-1 地基（风险①）。
2. 叶 + 映射：`MESSAGE_OUTBOUND_ACTION` 常量 + pin 测试（aero-common）→ `governance.rs` re-export + `[PROPOSED]` 注释改 landed 引用（aero-ai）。
3. aero-storage：`append_message_create_audit_in_tx` / `append_message_edit_audit_in_tx` helper + 接入 `insert_outboxed`（R1）与 `edit_outboxed_authorized`（R2）。
4. `migrations/0242_audit_governance_l1_aggregate.sql`（R3，含文本 cross-pin 注释）——**`cargo build` 再 migrate**（迁移编译期嵌入，AGENTS §4.2）。
5. db_tests：AC1/AC2/AC3（`to_regprocedure` 探测门）。
6. drill 腿：relay + t11 混合形态种子（AC4）。
7. 门禁：`cargo test --workspace --lib`（含 `-- --ignored` throwaway 库）+ `cargo clippy --workspace --all-targets` + `scripts/{truth-check,file-size-check,web-check}.sh` + `scripts/test-integration.sh` B5 段全 PASS（AC5）。
