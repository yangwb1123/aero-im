# Requirements Spec — aero-im-core B5-1：ImService message.*/room.* 写路径同事务审计 + 治理 outbox 入队 seam（producer side，handoff H3）

- **Module (analysis root)**: `crates/aero-im-core/src`（ImService 写路径 = 生产者契约面）；交付面含 `crates/aero-storage`（tx seam 接线 + H3 落点）、`crates/aero-ai/src/governance.rs`（lane 映射权威）、`crates/aero-common/src/model/audit.rs`（叶子 token 词表）
- **Direction**: "In-tx audit + governance-outbox enqueue seam for ImService message.*/room.*/admin.* operations (B5-1 producer side, handoff H3)"（value 10 / risk_reduction 9 / effort 7 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-im-core-src-e8fa8dff.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`
- **Sibling specs（同批次，边界协调）**: `2026-08-08-aero-common-b5-1-lane-mapping-in-tx-outbox.req.md`（共享基础设施腿：H3 repo 形状、无条件入队门语义、truth-check 守卫——**未落地**，本 spec 的验收 token 词表与其有差异，§6 决议）、`2026-08-08-aero-im-core-b5-1-in-tx-audit-governance-enqueue.req.md`（cfe64e80 分析的 im-core spec，更宽 token 集，未落地）、`2026-08-08-aero-ai-b5-1-migration-0239-governance-outbox.req.md`（0239 落地 spec，已完成）
- **Status**: Requirements（下述证据全部经源码 grep 核对，2026-08-08；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

> ⚠️ **direction 事实性更正（§1.1 逐条）**：① `delete_message` **不是**「无审计」——`soft_delete_outboxed_authorized`（message/authorization.rs:454）已向 `soft_delete_locked_outboxed_in_tx` 传 `Some("message.deleted")`，in-tx audit 行**已存在**；缺的只是 outbox 行。② `recall_message` 同理已 in-tx 写 `message.recalled`（authorization.rs:364），且**不在本 direction 验收的五个 token 内**（§3 出范围）。真正的零审计缺口 = `send_message`、`edit_message`、`create_room_in_workspace`、`add_member`、`archive_channel`、pin 族。③ 验收点名的 `moderation_finalize_without_binding_aborts_tx` 在落库 repo 中**不是独立测试**——其负例是 `moderation_finalize_outbox_parity` 内部的 rollback half（本 spec 的镜像目标，§5 A2）。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-im-core/src/service/messages.rs`（send_message_inner:139-248 / delete_message:430-447 / moderate_delete:611-647） | ✅ **Verified（行号微漂）**。`send_message_inner` :139-248，`insert_outboxed` 调用点 **:216**（direction 引 :246 系旧行号）；`delete_message` :430-447，`soft_delete_outboxed_authorized` 调用点 :447；`moderate_delete` :611-647，:645 调 `soft_delete_outboxed_system(…, Some("message.moderated"), …)`——**ImService 全文件零 `AuditRepo`/`audit_events` 引用**（messages.rs:1 grep 命中仅为 R-D1 注释）。`moderate_delete` 已含 R-D1 拒绝 `workspace=None`（:619-627） |
| E2 | `crates/aero-storage/src/audit_governance.rs:22`（'AuditGovernanceOutboxRepo lands in a sibling slice' — absent today） | ✅ **Verified**。模块 doc :22-24 原文：「The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a sibling slice (handoff H3 of the 0239 design doc); this module carries the DDL-contract fixtures only — **add the repo to this same module when it lands**」。`rg AuditGovernanceOutboxRepo crates/` 全仓唯一命中 = 该 doc——**repo 不存在**。文件现 **1141 行**（>800 WARN，距 1200 HARD 仅 59 行，§6 尺寸决议） |
| E3 | `crates/aero-storage/src/message/events.rs`（append_in_tx pattern、`soft_delete_outboxed_system` 同事务写 audit+outbox） | ✅ **Verified**。`soft_delete_outboxed_system` :267-292（begin/commit 自有 tx）→ `soft_delete_locked_outboxed_in_tx` :293-355：软删 UPDATE → :337 `AuditRepo::append_in_tx`（**返回值被丢弃**——本 direction 的 delete 接线点）→ `RoomEvent::Deleted` event outbox 同 tx。audit 行由 0239 AFTER INSERT 触发器在**同一 PG 事务**内产 outbox 行（moderation 专用） |
| E4 | `crates/aero-storage/src/audit.rs`（`AuditRepo::append_in_tx:127`） | ✅ **Verified（:127-136）**。签名 `append_in_tx(tx, workspace, actor: Option<ParticipantId>, action, target: Option<&str>, detail) -> Result<AuditId>`——**返回的 AuditId 即 outbox 行 event_id（1:1 前提）**；`append_on` 共享 INSERT :138-167 |
| E5 | `migrations/0239_audit_governance_outbox.sql`（token-keyed trigger，R2：仅 'message.moderated' 入队） | ✅ **Verified**。`aero_enqueue_governance_audit()`：Gate 1（runtime disabled → RETURN NEW）→ token 门（`NEW.action <> 'message.moderated'` → RETURN NEW，fail-open 零 RAISE）→ Gate 2（binding RAISE 仅 moderation 行）→ INSERT class='admin'/priority=100 + `ON CONFLICT (event_id) DO NOTHING`。DDL CHECKs：`class IN ('admin','message','room')`、`priority > 0`、`delivery_mode IN ('push')`、`status IN (0,1,2,3)`、`jsonb_typeof(payload)='object'`。**SQL 侧 token 集 = 仅 moderation——message.*/room.* 车道的 outbox 行只能走 Rust 显式写路** |
| E6 | `crates/aero-ai/src/governance.rs`（governance_lane_for token map） | ✅ **Verified（:74-102）**。唯一 match 臂 = `LOCAL_ACTION_MODERATED` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`，`_ => None`。`GOVERNANCE_PRIORITY_{MODERATION=100,BACKLOG=10}` :31/:33；re-export 链 :41-47。单测 6 项：`unknown_local_token_passes_through_unmapped` 把 `room.create`/`message.create`/`message.edit`/`message.deleted`/`call.join`/`""` 全部断言 None；`user_delete_token_stays_out_of_admin_lane` 钉 `message.deleted → None`（R-D2）——**这两个测试随本 direction 换形（§4 R2）** |
| E7 | （补充）`insert_outboxed` 无审计 | ✅ **Verified**。`MessageRepo::insert_outboxed`（message/idempotency.rs:158-236）：tx 内 `lock_effective_sender_room_access` → `insert_row_in_tx`（crud.rs:68）→ event_outbox INSERT → side-effect INSERT——**零 audit 引用**。生产调用者 = im-core `send_message_inner`（messages.rs:216）唯一 |
| E8 | （补充）edit/recall 路径审计现状 | ✅ **Verified**。`edit_outboxed_authorized`（message/authorization.rs:399-453）→ `edit_locked_outboxed_in_tx`（events.rs:69-140）：**零 audit**（grep `append_in_tx` authorization.rs 唯一命中 :364 = recall）。`recall_locked_outboxed_in_tx`（authorization.rs:296-374）：已 in-tx 写 `"message.recalled"`（:364-372，actor=recaller、target=message id、detail={room_id,digest}），**丢弃 AuditId** |
| E9 | （补充）room 写路径零审计 | ✅ **Verified**。`RoomRepo`（crates/aero-storage/src/room.rs + room/）零 `append_in_tx`。`create_in_workspace_authorized`（room/governance.rs:108-153，tx 内 room INSERT + owner member INSERT，workspace 即入参）；`add_member_authorized`（room/governance.rs:173-218，tx 内 `lock_room_aggregate` 解析 workspace + `ON CONFLICT DO NOTHING` 返回 bool）；`set_channel_archived_authorized`（room/governance.rs:396）。im-core `create_room_in_workspace`（service/room.rs:63-99）→ `create_in_workspace_authorized`；`add_member`（:266-291）→ `add_member_authorized`；`archive_channel`（channels.rs:123）→ `set_channel_archived_authorized` |
| E10 | （补充）harness 槽位与负例名 | ✅ **Verified**。`scripts/test-integration.sh:318-325`：`audit_governance::` 与 `moderation_finalize_outbox_parity` 两个 `run_migrated_integration` 条目（`cargo test -p aero-storage --lib --locked <filter> -- --ignored --test-threads=1`，**空过滤守卫** `test result: ok. [1-9][0-9]* passed`）；`moderation_finalize_without_binding_aborts_tx` **全仓不存在**——该负例是 `moderation_finalize_outbox_parity` 内的「Rollback half (A1-RB)」（删 binding → `moderate_finalize` Err → 软删回滚 + 0 audit + 0 outbox）。`scripts/b5-pin.sh:38-41` 钉名：`moderation_finalize_outbox_parity` / `a3-relay-drill` / `t11-fail-closed` / `moderation-priority-drill`；priority drill 的 `moderation-in-first-batch` PASS 在 `aero-audit-connector/src/bin/aero-audit-priority-drill.rs:276` |

### 1.1 对 direction 陈述的勘误/钉化（evidence-backed）

- **更正① delete 已审计，增量 = outbox 行**：`delete_message` → `soft_delete_outboxed_authorized`（authorization.rs:454-507）在事务内经 `lock_effective_message_write_access` 解析 `access.workspace`，以 `Some(actor)`/`Some("message.deleted")` 走 `soft_delete_locked_outboxed_in_tx`（events.rs:337 条件 `append_in_tx`）——audit_events 行**已 in-tx 提交**（crud.rs:374 先例 `soft_delete_audited` 同 token）。本 direction 对 delete 的增量 = **捕获 AuditId + 补 outbox 行**（同 tx）。
- **更正② recall 已审计且出范围**：`recall_locked_outboxed_in_tx`（authorization.rs:296-374）已 in-tx 写 `message.recalled` audit 行。direction 问题陈述提 recall 但**验收只点名五个 token**（message.send/message.edited/message.deleted/room.member.add/room.created）——recall/pin/archive_channel 均不在验收集（§3 出范围，保持 unmapped pass-through，兼作 R-D2/T-11 负例）。
- **更正③ 负例测试名**：`moderation_finalize_without_binding_aborts_tx` 在落库 repo 中不存在（E10）；镜像目标 = `moderation_finalize_outbox_parity` 的 rollback half（同名 harness 过滤器内，行为逐字可复制）。
- **行号漂移登记**：messages.rs:246（→:216，insert_outboxed 调用点）、:632（→:645，soft_delete_outboxed_system 调用点）；audit_governance.rs:22（模块 doc，不变）。**符号锚点全部命中**。
- **「audit_governance.rs:22 的 H3 手记」精确化**：模块 doc 明言 repo 落**本模块**（"add the repo to this same module when it lands"）——但文件 1141 行距 1200 HARD 仅 59 行，repo（~40 行）+ 新 parity 测试（~250 行）无法同文件落地 → **目录化拆分**（§4 R3/R5，§6 尺寸决议），模块路径 `audit_governance::` 保持（harness 过滤器逐字不变）。

## 2. Verified current state（缺口盘点）

```
入队两条路（E5）：
a) 触发器路   audit_events AFTER INSERT → aero_enqueue_governance_audit()
             └─ 仅 'message.moderated' → outbox（class 'admin'/priority 100）
                其余 token → RETURN NEW pass-through（fail-open 零 RAISE）
b) 显式写路   AuditGovernanceOutboxRepo —— 尚不存在（H3 手记 E1/E2）

ImService 写路径审计现状（E1/E7/E8/E9，全部 verified）：
  send（send_message_inner → insert_outboxed）  → 零 audit 行、零 outbox 行
  edit（edit_message → edit_outboxed_authorized）→ 零 audit 行、零 outbox 行
  delete（delete_message → soft_delete_outboxed_authorized）
                                                → audit 行 in-tx（message.deleted）；outbox 行 = 0
  moderate（moderate_delete → soft_delete_outboxed_system）→ audit + outbox 全（trigger）——唯一完整路径
  create_room_in_workspace / add_member / archive_channel / pin / unpin
                                                → 零 audit 行、零 outbox 行

缺口（本 direction 关闭）：
  a) send/edit 无 audit_events 行（B5 relay 与审计轨迹对消息生命周期不可见）
  b) create_room_in_workspace / add_member 无 audit_events 行（room 生命周期不可见）
  c) delete 有 audit 行但无 outbox 行（B5 relay 不可见）——其余四 op 两者皆无
  d) 无任何 in-tx oracle 证明「操作提交 ⇔ 恰 1 audit 行 + 恰 1 status-0 outbox 行同事务存在」
     （T-11/relay drills 直插种子行——never-firing producer 对它们不可见，vacuous green 风险，E10）
```

## 3. Scope

**In scope（= 验收的五个 op × 两个产出）**：

| ImService op | storage producer seam（已 verified） | 本地 token | 审计行现状 |
|---|---|---|---|
| `send_message` / `send_message_idempotent` | `insert_outboxed`（message/idempotency.rs:158） | `message.send` | 无 → 新增 |
| `edit_message` | `edit_outboxed_authorized`（message/authorization.rs:399） | `message.edited` | 无 → 新增 |
| `delete_message` | `soft_delete_locked_outboxed_in_tx`（message/events.rs:293，经 `soft_delete_outboxed_authorized`） | `message.deleted` | **已有** → 补 outbox |
| `create_room_in_workspace` | `create_in_workspace_authorized`（room/governance.rs:108） | `room.created` | 无 → 新增 |
| `add_member` | `add_member_authorized`（room/governance.rs:173，inserted=true 才写） | `room.member.add` | 无 → 新增 |

- 每个 op = 同事务「① 域变更 → ② `AuditRepo::append_in_tx`（返回 AuditId）→ ③ `AuditGovernanceOutboxRepo::append_in_tx`（event_id=AuditId，class/priority 按 lane，payload=16-key envelope）」；任一失败 → 整事务回滚（0 行逃逸）。
- `AuditGovernanceOutboxRepo`（H3，落 `crates/aero-storage/src/audit_governance/` 模块）+ 门语义（无条件入队）+ 叶子 5 个 `LOCAL_ACTION_*` token 常量 + truth-check 守卫扩展。
- `governance_lane_for`（aero-ai）扩 5 臂 + 受影响单测换形（`message.deleted` 从 None → message lane，**R-D2「永不 admin」保留**）。
- 验收 oracle db_tests：`moderation_finalize_outbox_parity` in-tx oracle 模式扩到五个 lane（aero-storage audit_governance 模块，§4 R5）。
- 既有钉位测试的受控更新：仅 `governance.rs` 两个单测（§4 R2）+ 零存储侧钉位测试改动（§4 R7 论证）。`moderation_finalize_outbox_parity` 名称/字面量**不动**。

**Out of scope**：
- `archive_channel` / `recall_message` / `pin_message` / `unpin_message` / `set_channel_meta` / slowmode / reaction-limit / retention——问题陈述提及但**验收未点名**；保持零 audit、unmapped pass-through（R-D2/T-11 负例语义不变）。recall 已有 audit 行但**不补 outbox 行**（token 不在验收集）。
- 0239/0240/0241 DDL 与 trigger、connector claim SQL/reconciler——**零改动**（trigger 仍是 `message.moderated` 唯一 SQL 生产者；0241 reconciler 保持 token-keyed）。
- v1（0236 `snaplink_delivery_outbox`）共存与重定向——不改（新 audit 行照常产 v1 行，cutover 窗口语义）。
- `message.react`、`join_channel`/`leave_channel`、legacy `create_room`（nil workspace）、系统编辑（unfurl/transcribe 的 `edit_outboxed_system`，events.rs:42，无 workspace 参数——**仅授权路径审计**）。
- L1 聚合（[PROPOSED]）、新 harness 腿 / 新 37-slot / 新迁移——**零新增**。
- ImService 签名/调用形状变更——**零**（seam 全在 storage tx 内，服务层零改动）。

## 4. Requirements

### R1 — 叶子 token 词表（`crates/aero-common/src/model/audit.rs`，action-token 区 :149-156 后）

新增 5 常量（值与验收逐字一致；doc 同 `LOCAL_ACTION_MODERATED` 纪律：「本地审计 token，生产调用点一律经常量拼写，绝不裸字面量」）：

| 常量 | 值 | 车道 class |
|---|---|---|
| `LOCAL_ACTION_MESSAGE_SEND` | `"message.send"` | message |
| `LOCAL_ACTION_MESSAGE_EDITED` | `"message.edited"` | message |
| `LOCAL_ACTION_MESSAGE_DELETE` | `"message.deleted"` | message |
| `LOCAL_ACTION_ROOM_MEMBER_ADD` | `"room.member.add"` | room |
| `LOCAL_ACTION_ROOM_CREATED` | `"room.created"` | room |

- `message.deleted` 是既有生产字面量（crud.rs:378 / events.rs:622 / authorization.rs:499）——常量化后生产点换常量；**不是新语义**。
- 不新增出站契约 token（`MODERATION_OUTBOUND_ACTION` 仍是唯一出站 token）；payload `action` = 本地 token 原样（half-B drill 先例 `back.action == "message.deleted"`，audit_governance.rs:534；v1 路径同）。
- `scripts/truth-check-lib.sh`：AUDIT-FLAG 守卫（现仅 `admin.content.flag`）扩为 token 集扫描——5 个新 token 逐个 `rg -n -F '"<token>"' crates --glob '*.rs'`，命中须 ∈ allowlist（叶子定义文件豁免）；audit.rs 之外的裸字面量 = 违规计入 exit 码（sibling aero-common spec R2 同形，消费其守卫机制）。

### R2 — `governance_lane_for` 扩 5 臂（`crates/aero-ai/src/governance.rs`）

- 新增 5 个 match 臂：`LOCAL_ACTION_MESSAGE_SEND/EDITED/DELETE` → `{class: GOVERNANCE_CLASS_MESSAGE, priority: GOVERNANCE_PRIORITY_BACKLOG(10), outbound_action: <本地 token 自身>, status: 0}`；`LOCAL_ACTION_ROOM_MEMBER_ADD/ROOM_CREATED` → 同形但 `class: GOVERNANCE_CLASS_ROOM`。**admin 车道保持仅 `message.moderated`**（`is_admin_class` 对 5 个新 token 全 false）。
- 单测换形（**仅此两处**）：
  - `unknown_local_token_passes_through_unmapped`（:126）：从负例集移除 `message.deleted`（现映射到 message lane），其余保持（`room.create`/`message.create`/`message.edit`/`call.join`/`""` 仍 None——**sibling 词表的 token 保持 unmapped**，§6 决议）。
  - `user_delete_token_stays_out_of_admin_lane`（:152）：`assert_eq!(governance_lane_for("message.deleted"), None)` 改为 `Some(lane)` 且 `lane.class == GOVERNANCE_CLASS_MESSAGE` + `!is_admin_class("message.deleted")`——**R-D2 不变量（永不 admin）以更强形式保留**；doc 注释同步。
  - 新增 per-token 映射 pin 测试（5 token 各 → class/priority=10/outbound=自身/status=0，DESC 语义注释同 `moderation_lane_preempts_backlog_under_desc_claim`）。

### R3 — `AuditGovernanceOutboxRepo`（H3，落 `crates/aero-storage/src/audit_governance/`）

**尺寸决议（§6 详述）**：`audit_governance.rs` 现 1141 行，repo + 新测试无法同文件落地 → 目录化拆分：

```
crates/aero-storage/src/audit_governance.rs    →  audit_governance/mod.rs（模块 doc + repo + 重导出）
crates/aero-storage/src/audit_governance/
    ├── mod.rs          模块 doc（含 H3 手记原文）+ AuditGovernanceOutboxRepo（本 R3）
    ├── db_tests.rs     既有 1141 行 DDL-contract fixtures（逐字搬移，零改动）
    └── producer_tests.rs  新五 lane parity 测试（§4 R5）
```

- 测试路径保持 `audit_governance::db_tests::…` / `audit_governance::producer_tests::…`——harness 过滤器 `audit_governance::`（子串匹配全路径）**逐字命中**；`lib.rs` 的 `pub mod audit_governance;` 不变。机械搬移，无行为变更（搬移后先跑 `audit_governance::` 既有 6 测试确认零回归）。

Repo 形状（镜像 `AuditRepo::append_in_tx` 形态，audit.rs:127）：

```rust
impl AuditGovernanceOutboxRepo {
    pub fn new(pool: PgPool) -> Self;
    /// 事务内写 outbox 行（0239 表）。event_id = audit_events.id（1:1）。
    /// INSERT … ON CONFLICT (event_id) DO NOTHING（重放/未来 trigger 双写幂等，
    /// duplicate_event_id_is_deduped_by_on_conflict 语义）。
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        class: &str,          // GOVERNANCE_CLASS_*（0239 CHECK 值域）
        priority: i16,        // 10 = GOVERNANCE_PRIORITY_BACKLOG（comment-pin）
        payload: serde_json::Value, // AuditClaimPayload::new 产物（jsonb object CHECK）
    ) -> Result<(), sqlx::Error>;
}
```

- INSERT 形状逐字镜像 half-B（audit_governance.rs:544-548）：仅 `(event_id, class, priority, payload)` 四列——`status`/`available_at`/`created_at`/`attempts` 走 DDL 默认（0/clock_timestamp()/0，`ddl_contract_defaults_and_checks` 钉的 drill 形状），T-11 claim 谓词兼容。
- payload 构建：同模块共享 helper（half-B :531-542 先例）——`AuditClaimPayload::new(event_id, source_system, occurred_at, actor, targets, aggregate_id, action, detail)`；`occurred_at` 必须 `SELECT to_jsonb(created_at) #>> '{}' FROM audit_events WHERE id=$1` 从 PG 取拼写（**绝不** Rust formatter）；`actor` = `AuditActor::participant`（human）/`system`（系统路径）；`targets` = `[AuditTarget::resource(target)]`；`aggregate_type='workspace'`、`aggregate_id` = workspace、`action` = 本地 token 原样、`outcome='success'`、`data_classification='confidential'`、`retention_class='security'`（0239 envelope 逐字段，half-B 断言集）。
- `source_system` 规则（sibling aero-common R4 同款）：workspace 有 binding 时取其 `source_system`（`SELECT source_system FROM snaplink_commercial_bindings WHERE workspace_id=$1`），否则回退常量 `"aero-im"`（叶子 sample_payload 同源，model/audit.rs:455）；**无 binding 不 raise、不入商业门**。
- **门语义 = 无条件入队**（sibling aero-common R6 同款）：Rust 显式写路**不 consult `snaplink_commercial_runtime.enabled`**、不做 binding RAISE——与 0239 trigger 的 Gate 1/Gate 2 是**刻意不对称**（trigger 门属 moderation SQL 路专属；app 侧 delivery 契约 = 行必落 status 0，投递门在 connector）。0241 reconciler 零改动（仍仅 `message.moderated` 自愈——Rust 路行在写时已落，无需 backfill）。

### R4 — 生产者 seam 接线（5 条，全部 in-tx）

统一模式（每条 seam 在其既有事务内追加）：① 域变更 → ② `AuditRepo::append_in_tx`（返回 AuditId；**delete 捕获现被丢弃的返回值**）→ ③ `AuditGovernanceOutboxRepo::append_in_tx`。任一失败 → 整事务回滚（0 行逃逸，镜像 crud.rs:359-363「a message is never silently deleted unaudited」）。token 经叶子常量拼写（R1 守卫）。

| 本地 token | seam（已 verified） | 接线要点 |
|---|---|---|
| `message.send` | `insert_outboxed`（message/idempotency.rs:158，tx 内） | workspace 经 `SELECT workspace_id FROM rooms WHERE id=$1` in-tx 解析（room 已被 `lock_effective_sender_room_access` 锁行；`rooms.workspace_id` NOT NULL）；actor = sender；target = message id；detail = `{"room_id": …}`；幂等重放命中（`find_outboxed_by_client_message_id` 早退路径）不写（无域变更） |
| `message.edited` | `edit_outboxed_authorized`（message/authorization.rs:399，tx 内，`access.workspace` 已解析） | audit+enqueue 插在 `edit_locked_outboxed_in_tx` 返回 `Ok(Some)` 后、`tx.commit()` 前；actor = actor；target = message id；detail = `{"room_id": …, "digest": …}`；version 冲突 / 已删 / 已 recall（`Ok(None)`）不写。**系统编辑 `edit_outboxed_system`（unfurl/transcribe，events.rs:42）零改动**——无 workspace 参数，不审计（§3） |
| `message.deleted` | `soft_delete_locked_outboxed_in_tx`（message/events.rs:293，:337 条件 `append_in_tx` 处） | 捕获 AuditId；**仅当 audit token = `message.deleted` 时补 outbox 行**；`message.moderated`（`soft_delete_outboxed_system` moderation 路径）**跳过**——trigger 是该 token 唯一 outbox 生产者（moderation 不得双写，R5 硬线；即便双写也撞 PK 走 DO NOTHING 无害，但语义上触发器独占）。`workspace=None`/`audit_action=None`（无审计分支）不写。`crud.rs` 的 `soft_delete_audited`/`soft_delete_moderated`（测试专用，E10）**零改动**——`non_moderation_action_passes_through_unmapped` 等钉位测试逐字保持（§4 R7） |
| `room.created` | `create_in_workspace_authorized`（room/governance.rs:108，tx 内，workspace 即入参） | audit+enqueue 在 room INSERT + owner member INSERT 后、`tx.commit()` 前；actor = creator；target = room id；detail = `{"room_id": …, "kind": …, "name": …}`。`RoomRepo::create_in_workspace`（room.rs:267，legacy 裸路径）**零改动**（非 ImService 入口） |
| `room.member.add` | `add_member_authorized`（room/governance.rs:173，tx 内，workspace 经 `lock_room_aggregate` 解析） | **仅 `inserted == true`（`ON CONFLICT DO NOTHING` 实际插入）时写**；actor = caller；target = room id；detail = `{"room_id": …, "member": …}` |

- **fail-closed 规则**：seam 内 workspace 解析失败 / audit 写失败 / enqueue 写失败 = `Err`（整事务回滚），**绝不静默 skip**——「unknown workspace aborts the tx」的服务端实现（§5 A2 负例）。
- ImService 层（`crates/aero-im-core/src/service/`）**零改动**：调用形状冻结，seam 全在 storage tx 内（E7/E8/E9 锚点）。

### R5 — 验收 oracle db_tests（`audit_governance/producer_tests.rs`，harness 过滤器双命中）

新测试命名带 `moderation_finalize_outbox_parity_` 前缀（子串匹配：同时命中 `audit_governance::` 与 `moderation_finalize_outbox_parity` 两个 harness 条目——**扩展在钉名过滤器下非空执行**）。每测试三分部（镜像 `moderation_finalize_outbox_parity` 的 Commit/Rollback/Replay 结构，E10）：

- **提交半部**：fixture = 既有 `fixture()`/`message_in_workspace` 形态（workspace + actor + room；`MessageRepo::insert` 种子——该路径无 seam，不产 audit，隔离干净）。驱动五个生产 seam 各一 op 后断言（**workspace 作用域**，共享 DB 纪律）：
  1. `audit_events` 恰 1 行：`workspace_id` = fixture ws、`action` = 词表 token（R1 常量）、`actor_id` = 操作者、`target` = message id / room id；
  2. `audit_governance_outbox` 恰 1 行（`payload->>'aggregate_id'` = ws）：`status = 0`、`event_id = audit_events.id`（1:1）、`class`/`priority` 按 R2 lane（message/room + **10**，comment-pin `GOVERNANCE_PRIORITY_BACKLOG`——aero-storage 不得 import aero-ai 的既有惯例）、`payload.event_id == audit_events.id`（验收原句）；
  3. **T-11 形状**（非空证明，镜像 aero-audit-t11-drill 语义）：`attempts = 0`、`last_error IS NULL`、claim 谓词镜像（`status IN (0,1) AND available_at <= clock_timestamp() AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())`，pg.rs:104-117）选中该行——relay 缺席时行保持 status 0 pending，**绝不假成功/假死**；
  4. 提交半部运行于 **enforcement OFF（fresh-DB 默认）且无 binding**——同时证明无条件入队（R3 门语义：runtime disabled 也产 outbox 行）与 `source_system` 回退 `"aero-im"`。
- **回滚半部**（负例，镜像 parity 的 A1-RB）：置 `snaplink_commercial_runtime.enabled = TRUE`（全局 singleton，**测后必 restore**）且 fixture ws **无 binding** → 同一 op 返回 `Err`（v1 0236 `audit_events_snaplink_delivery` trigger 对 enforcement-on + 缺 binding 的 audit INSERT RAISE `commercial binding is unavailable`，0236:68-84——每条 seam 都写 audit_events，**全 op 统一可注入失败**；send 另可能先被 0235 metering trigger 拦截，同为 fail-closed）→ 断言**域行 0 逃逸**（消息未插入/未删除/未编辑/房间未建/member 未加）+ `audit_events` 0 行 + outbox 0 行——**ROLLBACK 移除两者（无 orphan outbox）**。
- **重放/幂等半部**：已提交 op 的重放（delete 已删 → `Ok(None)`、send 幂等键命中、add_member 已存在 → `Ok(false)`）不增行（audit 与 outbox 计数不变）。
- 测序纪律：每测试开头 `reset_governance_table`（既有 helper）+ 防御性 re-assert enforcement OFF；结尾 `restore_enforcement_disabled`（audit_governance.rs:169-182 先例；全局 singleton 共享，harness `--test-threads=1` 串行）。

### R6 — harness 槽位保持（验收「Keep … green」）

| 槽位（scripts/b5-pin.sh:38-41） | 保持方式 |
|---|---|
| `audit_governance::` | 新测试在 `audit_governance::producer_tests` 模块 → 过滤器命中 ≥1（空过滤守卫） |
| `moderation_finalize_outbox_parity` | 既有 parity 测试**名称/字面量不动**（钉名契约，audit_governance.rs doc :12-19）；新测试前缀命中同名过滤器 |
| `a3-relay-drill` | connector/DDL 零改动（R7），drill 自种种子行，不受 producer 影响 |
| `t11-fail-closed` | 同上；新 seam 产出行 T-11 形状（R5-3） |
| `moderation-priority-drill`（`moderation-in-first-batch` PASS） | claim 排序零改动（B5-3 已落地）；新行 priority=10 ≤ 100，不影响 moderation 优先；drill 在 fresh DB 自种 500+1 行，producer 不参与 |

### R7 — 零改动清单与钉位测试保持

- 零改动：`migrations/0239/0240/0241`、`aero-audit-connector`（pg.rs/relay.rs/drills）、`main.rs` boot、`aero-eng`、0241 reconciler（token-keyed 于 `message.moderated` 不变——新 lane 行在写时已落，无 disabled-window 缺口）。
- 钉位测试保持（逐字）：`moderation_finalize_outbox_parity`、`moderation_finalize_runtime_disabled_commits_1_plus_0`、`non_moderation_action_passes_through_unmapped`（用 `soft_delete_audited`——无 seam，零 outbox 行语义不变）、`governance_reconcile_backfills_disabled_window`（用 `soft_delete_audited` + `soft_delete_outboxed_system` moderation 路径）、`duplicate_event_id_is_deduped_by_on_conflict`、`ddl_contract_defaults_and_checks`、`rust_produced_payload_matches_0239_envelope`。
- v1 共存不变：新 audit 行照常产 `snaplink_delivery_outbox` v1 行（parity 测试 half 4 语义对五 lane 同样成立，cutover 窗口行为）。

## 5. Testable acceptance mapping（direction acceptance 原句保留 + 可测试化）

> **Acceptance 原文**：Extend the `moderation_finalize_outbox_parity` in-tx oracle pattern (crates/aero-storage/src/audit_governance.rs db_tests, harness filter 'moderation_finalize_outbox_parity') to message.send/message.edited/message.deleted/room.member.add/room.created: same-tx assert exactly one audit_events row AND one audit_governance_outbox row with status=0, class/priority matching governance_lane_for (backlog 10), payload.event_id == audit_events.id; ROLLBACK removes both (no orphan outbox). Negative: an op with an unknown workspace aborts the tx (fail-closed), mirroring `moderation_finalize_without_binding_aborts_tx`. Keep harness b5_check slots green: relay-drill + t11-fail-closed (T-11: rows stay status=0 pending, never falsely dead) unchanged, moderation-priority-drill `moderation-in-first-batch` PASS.

| AC（原句分句） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **(a)** extend the `moderation_finalize_outbox_parity` in-tx oracle pattern to message.send/message.edited/message.deleted/room.member.add/room.created | 新 db_tests（`moderation_finalize_outbox_parity_*` 前缀，§4 R5 提交半部）：五 op 各驱动其生产 seam（E7/E8/E9 锚点），同事务断言恰 1 `audit_events` 行（token/actor/target/workspace_id 全字段）+ 恰 1 outbox 行。**harness 过滤器双命中**：`audit_governance::` 与 `moderation_finalize_outbox_parity` 两条目均执行新测试（空过滤守卫要求 ≥1 真实匹配） | `crates/aero-storage/src/audit_governance/producer_tests.rs` |
| **(b)** status=0, class/priority matching governance_lane_for (backlog 10), payload.event_id == audit_events.id | 每行断言 `status=0`（DDL 默认，`ddl_contract_defaults_and_checks` 钉）；`class` == 叶子 `GOVERNANCE_CLASS_MESSAGE`/`GOVERNANCE_CLASS_ROOM`（R2 链：governance.rs 用同叶子常量 → 行值 == 叶子 == `governance_lane_for` 值）；`priority` == 10（comment-pin `GOVERNANCE_PRIORITY_BACKLOG`，audit_governance.rs half-B :543-547 同款惯例）；`payload.event_id` == `audit_events.id`（字符串 1:1，half-B :577-605 断言集）。governance.rs 新增 per-token pin 测试（R2）钉 `governance_lane_for` 输出本身 | producer_tests.rs + governance.rs tests |
| **(c)** ROLLBACK removes both (no orphan outbox) | 回滚半部（§4 R5）：enforcement ON + 无 binding → 同 op `Err`（0236 RAISE，E10 镜像）→ 域行 0 + `audit_events` 0 + outbox 0——两行同生共死，无「audited but unqueued / deleted but unaudited」半态 | producer_tests.rs |
| **(d)** Negative: an op with an unknown workspace aborts the tx (fail-closed), mirroring `moderation_finalize_without_binding_aborts_tx` | **镜像目标勘误**：落库 repo 无独立同名测试——负例 = `moderation_finalize_outbox_parity` 的 rollback half（E10），本 spec 逐字镜像之（删 binding → Err → 三零断言）。另：seam 内 workspace 解析失败 = `Err` 不静默（R4 fail-closed 规则）；`create_in_workspace_authorized` 传不存在 workspace id → `lock_workspace`（room/governance.rs:589-601）`WorkspaceNotFound` Err + 零行（可构造实例） | producer_tests.rs |
| **(e)** Keep harness b5_check slots green: relay-drill + t11-fail-closed unchanged, moderation-priority-drill `moderation-in-first-batch` PASS | 零改动清单（R7）保证三 drill 行为不变；新 seam 行 T-11 形状（R5-3：status=0/attempts=0/claim 谓词命中，relay 缺席保持 pending）；新行 priority=10 不扰动 DESC 优先（moderation 100 仍先 claim）。验证 = `bash scripts/test-integration.sh` 全链：`a3-relay-drill`/`t11-fail-closed`/`moderation-priority-drill` 三条目 PASS | scripts/test-integration.sh（既有段，零改动） |

## 6. Coordination & hard rules（AGENTS §4）

- **token 词表决议（本 spec 验收优先）**：验收钉死 `message.send`/`message.edited`/`message.deleted`/`room.member.add`/`room.created`。sibling aero-common spec（未落地）与 cfe64e80 im-core spec（未落地）用 `message.create`/`message.edit`/`room.create`/`room.archived`/`message.recalled` 等——**集成时以本验收为准**：`governance_lane_for` 臂数 = 并集（本 5 臂 + sibling 落地时的臂），`unknown_local_token_passes_through_unmapped` 负例集保持 `message.create`/`message.edit`/`room.create`/`call.join`/`""`（sibling token 未落地前保持 unmapped）。shared 文件（model/audit.rs、governance.rs、audit_governance 模块）手接，勿并行整文件覆盖（AGENTS §4.1）。
- **目录化拆分是尺寸强制项**（file-size-check.sh：800 WARN / 1200 HARD，唯一权威）：audit_governance.rs 1141 行，repo（~40 行）+ 新测试（~250 行）必超 1200 HARD（超限文件**禁止修改**）。拆分为 `audit_governance/` 目录模块：既有 fixtures 逐字搬入 `db_tests.rs`（零改动），harness 过滤器 `audit_governance::` 子串命中不变；`lib.rs` 声明不变。搬移本身先于接线，单独提交可回滚（勿与 in-flight batch 的其它重构叠加——AGENTS §4.4 结构治理；本次搬移是尺寸硬约束下的最小机械动作）。
- **尺寸纪律其余**：`message/idempotency.rs`（1028）send seam ≤30 行；`room/governance.rs`（801）两 seam ≤40 行；`message/events.rs`（647）+ `authorization.rs`（510）delete/edit seam ≤30 行——全部 <1200 HARD。
- **迁移纪律**：本 direction **零迁移**（0239/0240/0241 已落地，勿改）；trigger 仍是 `message.moderated` 唯一 SQL 生产者（R-D2）。
- **零 harness 改动**：无新 37-slot、无新 integration 段；新测试走既有两条目（§4 R6）。
- **全局状态纪律**：回滚半部翻转 `snaplink_commercial_runtime` 全局 singleton——测后必 restore（audit_governance.rs 先例），断言一律 workspace/event_id 作用域（共享非空库跑 ignored 套件，`--test-threads=1`）。
- **字面量纪律**：新 Rust 代码不拼 token 字面量（R1 守卫扩展后全仓扫描）；`"admin.content.flag"` 仍只许 audit.rs:150。
- **提交前必过**：`cargo build`（无新迁移，但照例）· `cargo check --workspace` · `cargo test --workspace --lib`（全绿底线）· `cargo test --workspace --lib -- --ignored`（DATABASE_URL + 已迁移 throwaway 库；重点验证 audit_governance:: 族 6 既有 + 新测试）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `bash scripts/test-integration.sh`（三 drill 条目 PASS）。
- **活验证**：全新一次性库建库 → `aero-cli migrate` → 跑 `audit_governance::` 与 `moderation_finalize_outbox_parity` 两条目 → DROP；`make migrate-smoke` 不受影响（零迁移）。

## 7. Risks

- **审计行新增的连锁失败面（最高风险）**：send/edit 首获 audit 行后，enforcement ON + 无 binding 的 workspace 上 send/edit/delete 将开始 fail-closed（0236 RAISE / 0235 metering）——这是验收 (d) 的**预期行为**（镜像 moderation 已有语义），但对既有测试是回归面：任何「enforcement ON + 无 binding + 走 insert_outboxed/edit/delete」的存量 db_test 会新失败。缓解：全量 `-- --ignored` 跑查；若有命中，逐一点验其 fixture（多半已带 binding，如 `message_quota_and_snaplink_outboxes_are_transactional`）。
- **目录化拆分的回归面**：机械搬移 1141 行 fixtures——搬移后先单独跑 `audit_governance::` 6 测试确认零回归再继续；与 in-flight batch 不叠加其它结构动作。
- **词表并集漂移**：sibling spec 落地时若其 token 拼写（`message.create` 等）与验收不同，`governance_lane_for` 出现 10 臂（两套近义 token）——语义重复但无害（各自 lane 相同）；必要时由 campaign 决策收敛为一个拼写（出本 direction 范围）。
- **无条件入队 vs trigger Gate 1 的不对称**：runtime disabled 时 moderation 行不进 outbox（A2 half 4 钉）、message/room 行进（本验收钉）——有意设计（R3 门语义），若未来要求统一，改的是 0241 reconciler 或 trigger（出范围）。
- **im-core 服务边界未直接驱动 oracle**：验收 oracle 驱动 storage seam（与既有 moderation parity 同风格）；ImService 层零改动 + 调用形状冻结（E7-E9）保证 seam 即服务路径——若需 svc.* 级 drill，属 cfe64e80 im-core spec 范围（其 db_tests/audit_governance_tests.rs 计划）。
- **`moderation_finalize_without_binding_aborts_tx` 为仓外/旧名**：本 spec 以落库的 rollback half 为镜像对象（E10），引用旧名只作验收原句保留，不新增该名测试。

## 8. Sequencing

1. **拆模块**：`audit_governance.rs` → `audit_governance/{mod.rs,db_tests.rs}`（逐字搬移）→ `cargo test -p aero-storage --lib audit_governance:: -- --ignored`（DATABASE_URL）确认 6 既有测试零回归。
2. **叶子 + 映射**：model/audit.rs 5 常量（R1）→ truth-check 守卫扩展 → governance.rs 5 臂 + 2 单测换形 + per-token pin（R2）→ `cargo test -p aero-ai governance::` 全绿。
3. **H3 repo**：`audit_governance/mod.rs` 落 `AuditGovernanceOutboxRepo`（R3）——先纯仓储（含 half-B 复用），`cargo check` 过。
4. **5 条 seam 接线**（R4）：delete（最简，捕获 AuditId + 条件 enqueue）→ send → edit → room.created → room.member.add；每接一条跑对应 db_tests + 既有钉位测试（R7 清单）。
5. **验收 oracle**：`audit_governance/producer_tests.rs` 新测试（R5 三分部 × 五 op）→ 两条目 harness 过滤跑绿。
6. **全链**：`cargo test --workspace --lib -- --ignored`（重点查 §7 连锁失败面）→ `bash scripts/test-integration.sh`（三 drill 条目 PASS）→ clippy + 三脚本 0 违规。
7. **门禁提交**：拆分（步骤 1）单独提交；接线 + 测试一批提交；`git diff` 收敛（零迁移、零 connector 改动、零 harness 改动）。
