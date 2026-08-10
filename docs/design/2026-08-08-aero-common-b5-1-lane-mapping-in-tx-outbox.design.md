# Design — aero-common B5-1：message.*/room.*/admin.* 车道映射 + in-tx `AuditGovernanceOutboxRepo`（H3 交付）

- **Requirements**: `docs/requirements/2026-08-08-aero-common-b5-1-lane-mapping-in-tx-outbox.req.md`
- **Sibling（共享 H3 落点）**: `docs/design/2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.design.md`（auth 切片与本设计**同一** `AuditGovernanceOutboxRepo`，只落一次）
- **模块锚点**（稳定 grep 名，行号仅为核对时锚点）：`crates/aero-common/src/model/audit.rs` · `crates/aero-ai/src/governance.rs` · `crates/aero-storage/src/audit_governance.rs` · `crates/aero-storage/src/message/{crud,events,authorization,idempotency}.rs` · `crates/aero-storage/src/room.rs` + `room/governance.rs` · `scripts/truth-check-lib.sh`

> **Revision 1（2026-08-08，design-gate，audit_integrity finding 1）**：0236 v1-trigger RAISE 在 runtime-ON/no-binding 下的处置**定稿 = F7/R6 rescope + AC6 pin**（**不改触发器、不种 binding 掩盖**）：① 该态经受支持路径**库层不可达**——`configure_enabled` 与开关翻转同事务执行 `require_complete_workspace_coverage`（`snaplink_commercial.rs:453`：所有 `workspaces` 行含 nil 必须有 enabled binding）+ `reject_omitted_enabled_bindings`（:434），boot `from_env` fail-loud（`runtime.rs:42-70`；生产写开关仅 configure_enabled/configure_disabled/require_disabled 三 repo 方法，无 HTTP 端点）——D13 不变量，与批次 connector-root 分析一致；② 残余面（手动 SQL 直翻开关）下 audit INSERT 触发 0236 Gate 2 RAISE → 整事务 fail-closed 回滚，与**今天** message.create（0235 metering 门）/ message.delete（既有 audit 行）在该态的失败语义**逐字一致**——本设计不新增任何可达态失败（R6 保证面 = 显式写路本身 + 全部可达态）；③ fallback `"aero-im"` 仅 runtime OFF 可达（0236 Gate 1 pass-through）；④ **AC6 新 db_test**（直接 SQL 翻开关构造该态，镜像 connector-root F1 回归先例）把该态从「CI 盲区」变「钉死行为」。详见 F7/F14/D7/§6 AC6。

## 0. Evidence adjudication（untrusted claims → verified anchors）

本设计把 req 的 11 条证据（6 cited + 5 supplemental）逐条当**不可信声明**重验，全部实跑/源码核对于 2026-08-08：

| # | 声明 | 裁决 |
|---|---|---|
| E1 | `model/audit.rs` 仅 2 个 action token + `GOVERNANCE_CLASS_*` + "locked as ONE constant" doc | ✅ 实读 :141-168。`MODERATION_OUTBOUND_ACTION="admin.content.flag"`、`LOCAL_ACTION_MODERATED="message.moderated"`、`GOVERNANCE_CLASS_{ADMIN,MESSAGE,ROOM}` 派生自 `AuditClass::as_str()`；`AuditClaimPayload::new`（:295，8 可变入参）+ `AuditActor`/`AuditTarget` envelope twin 已落地 |
| E2 | `governance_lane_for` 单臂 match、`_ => None` | ✅ 实读 :74-86。`GOVERNANCE_PRIORITY_{MODERATION=100,BACKLOG=10}` :31/:33；**实跑 `cargo test -p aero-ai governance::` = 6 passed**（含 `unknown_local_token_passes_through_unmapped`、`user_delete_token_stays_out_of_admin_lane`，后者钉 `message.deleted → None`） |
| E3 | `audit_governance.rs:22` H3 手记、repo 未落 | ✅ 实读模块 doc :19-27；`rg AuditGovernanceOutboxRepo` 全仓唯一命中 = 该 doc |
| E4 | 0239 触发器仅分支 `message.moderated` | ✅ 实读 :83-84 `IF NEW.action <> 'message.moderated' THEN RETURN NEW`；Gate 1（runtime disabled pass-through）+ Gate 2（binding RAISE）+ `ON CONFLICT (event_id) DO NOTHING` |
| E5 | `AuditRepo::append_in_tx` :127 / `append_on` :154 | ✅ 实读 :129-167。`append_in_tx` 返回 `AuditId`（即 outbox `event_id` 1:1 前提）；**~25 个调用点**（bot/integration/announcement/scim/participant/deactivation…）——签名**不可改**（见 §7 D2） |
| E6 | aero-im-core 零 audit producers | ✅ `rg audit crates/aero-im-core/src` 仅注释命中；im-core 依赖 = aero-common + aero-storage |
| E7 | 5 条生产者 seam 现状 | ✅ 逐一实读：`insert_row_in_tx` crud.rs:68（**不是 req 写的 :63，行漂移**；insert :31 / insert_idempotent idempotency.rs:84 / insert_outboxed :158 三调用点共用内核）· `edit` crud.rs:168（单语句非 tx）· `edit_outboxed_authorized` authorization.rs:403（已有 tx，`lock_effective_message_write_access` 返回的 `access.workspace` 现被 `.is_none()` 丢弃——可捕获）· `soft_delete_audited` crud.rs:364（已 tx + audit in-tx，无 outbox）· `soft_delete_locked_outboxed_in_tx` events.rs:300（audit in-tx :337，无 outbox）· `soft_delete_moderated` crud.rs:398 / `soft_delete_outboxed_system` events.rs:267（trigger 独占，**不得双写**）· `create_in_workspace` room.rs:267（已 tx，workspace 即入参）· `create_in_workspace_authorized` room/governance.rs:108（**自持 INSERT，不调 create_in_workspace**，两处都要接线）· `set_channel_archived_authorized` :396（已 tx，workspace 来自 `lock_channel_aggregate`）· 裸 `archive` room.rs:437（单语句非 tx） |
| E8 | claim 排序契约 | ✅ pg.rs:117 `ORDER BY priority DESC, available_at, created_at, event_id`；b5-pin.sh:41 `moderation-priority-drill`；drill bin :283 `moderation-in-first-batch: PASS` |
| E9 | half-B drill 先例 | ✅ `rust_produced_payload_matches_0239_envelope`（audit_governance.rs:445 起）直插 class 'message'/priority 10 + `AuditClaimPayload::new` 信封过全部 0239 CHECK——**新车道 payload 形态已被证明合法** |
| E10 | harness 门 | ✅ test-integration.sh:313-323：`audit_governance::` + `moderation_finalize_outbox_parity` 两条目，0239 文件存在 → un-gated；空过滤器守卫禁 vacuous green |
| E11 | R-D2/T-11 pin | ✅ `user_delete_token_stays_out_of_admin_lane` :151-156；`non_moderation_action_passes_through_unmapped` :818；`governance_reconcile_backfills_disabled_window` :964（实读两测试全文，换形方案见 §6）；T-11 drill 在树 |

**基线实跑**（2026-08-08，本设计亲验）：`cargo test -p aero-common` = lib 112 passed / doc-tests 3 passed + 1 ignored；`cargo test -p aero-ai governance::` = 6 passed。与 req 状态块一致。

**新发现（req 之外的勘误，本设计钉定）**：

1. **req R5 分支判据字面不可行**：`soft_delete_outboxed_system` 的分支判据写作「`governance_lane_for(audit_action)` + `audit_action != LOCAL_ACTION_MODERATED`」，但 **aero-storage 不得依赖 aero-ai**（audit_governance.rs 模块 doc 明言依赖方向），`governance_lane_for` 定义在 aero-ai。本设计钉定可行等价物：aero-storage 内 `lane_params_for`（叶子常量 5 臂镜像，§2.2），由两侧测试双钉（aero-ai per-token pin + aero-storage parity db_test）。
2. **`GOVERNANCE_PRIORITY_*` 不在叶子**：priority 常量定义在 aero-ai（:31/:33），叶子只有 class 别名。storage 侧镜像沿用既有惯例（模块 doc 已 cross-pin `= 100`/`= 10` 字面量）——镜像用字面量 `10` + cross-pin 注释，不扩叶子（scope 红线）。
3. **`message.recalled` 已有 audit 生产者**：authorization.rs:368（recall 路径 in-tx 写 `"message.recalled"` audit 行，无 outbox 行）——是 AC5 的现成 unmapped 负例，且**不在** 5 token 守卫面（out of scope，其字面量不动）。
4. **`create_in_workspace_authorized` 自持事务**：不调 `create_in_workspace`（两条独立 INSERT 路径），req R5 表已列两者——本设计确认两者都需接线。
5. **行漂移**：`insert_row_in_tx` 在 crud.rs:68（req :63）、`AuditClaimPayload::new` 在 :295（req :238）、room 文件 = `crates/aero-storage/src/room.rs`（非 room/room.rs）。均不影响设计。

## 1. Design overview

```
                         ┌────────────── aero-storage（全部 in-tx 机制落家）──────────────┐
                         │  audit_governance.rs（H3 落点）                                 │
                         │    AuditGovernanceOutboxRepo::append_in_tx(tx,event_id,class,  │
                         │      priority,payload)  ← 与 auth sibling §2.1 同一签名         │
                         │    lane_params_for(action)  ← 叶子常量 5 臂镜像（非 aero-ai）    │
                         │    append_lane_pair_in_tx(tx,…)   ← audit + 条件 outbox         │
                         │    append_outbox_for_audit_in_tx(tx,…) ← 仅 outbox（delete 用） │
                         │    source_system 解析（binding 或 "aero-im"，不 RAISE）          │
                         └────────────────────────────────────────────────────────────────┘
  message.create   insert_row_in_tx（crud.rs:68，覆盖 insert / insert_idempotent / insert_outboxed）
  message.edit     edit（crud.rs:168 包 tx）+ edit_outboxed_authorized（authorization.rs:403 捕获 access.workspace）
  message.deleted  soft_delete_audited（crud.rs:364）+ soft_delete_locked_outboxed_in_tx（events.rs:300）
  room.create      create_in_workspace（room.rs:267）+ create_in_workspace_authorized（room/governance.rs:108）
  room.archived    set_channel_archived_authorized（archived=true 才写）+ 裸 archive（room.rs:437 包 tx）
      │ 每 seam：域变更 → AuditRepo::append_in_tx（→ AuditId）→ append_in_tx(outbox, event_id=AuditId)
      │ 同一事务；任一失败 → 整事务回滚（0 行逃逸，fail-closed）
      ▼
  audit_events ──0239 触发器──▶ message.moderated → outbox（admin/100，触发器独占，Gate 1/2 语义）
  （其余 token 触发器 pass-through，不 RAISE）      message.deleted 等 5 token → 由 Rust 显式写路入 outbox
                                                   （message/room class、priority 10、status 0、无条件，R6）
```

**两条入队路的边界（硬线）**：

- **触发器路（不变）**：`message.moderated` 是 0239 触发器**唯一** outbox 生产者（Gate 1 runtime 门 + Gate 2 binding RAISE + `ON CONFLICT DO NOTHING`）。Rust 显式写路**永不**写该 token（硬线，防双写语义混淆；即便误写也撞 PK 走 DO NOTHING 无害，但代码审查禁止）。
- **Rust 显式写路（本设计）**：5 个新 token 各在既有事务内「域变更 → audit 行 → outbox 行」，**无条件入队**（不 consult `snaplink_commercial_runtime.enabled`、无 binding RAISE，镜像 auth sibling R6——消息/房间生命周期不因商业开关或缺 binding 失败）；`source_system` = 该 workspace 的 enabled binding 值，无则回退 `"aero-im"`（保证面 = 显式写路本身 + 全部经受支持路径可达态；runtime-ON + 无 binding 的残余态语义见 F14/D7）。
- **共同交付物**：`AuditGovernanceOutboxRepo` 与 auth sibling 共享（全仓只落一次）；envelope 构建器 `governance_envelope` 亦共享（auth sibling §2.1 定稿签名，本设计消费）。

## 2. API changes（逐 crate 签名）

### 2.1 `crates/aero-common/src/model/audit.rs`（叶子 token 词表，R1+R2）

在 action-token vocabulary 区（:149-156 之后）新增 5 常量，doc 注明「本地审计 token，生产调用点一律经常量拼写，绝不裸字面量（truth-check 守卫，§2.6）」：

```rust
/// Local audit token produced by every message-create producer
/// (`MessageRepo::insert`/`insert_idempotent`/`insert_outboxed` kernel).
pub const LOCAL_ACTION_MESSAGE_CREATE: &str = "message.create";
/// Local audit token produced by every message-edit producer
/// (`MessageRepo::edit`/`edit_outboxed_authorized`).
pub const LOCAL_ACTION_MESSAGE_EDIT: &str = "message.edit";
/// Local audit token produced by every user-initiated message delete
/// (`soft_delete_audited`/`soft_delete_outboxed_authorized`).
pub const LOCAL_ACTION_MESSAGE_DELETE: &str = "message.deleted";
/// Local audit token produced by every room-create producer
/// (`RoomRepo::create_in_workspace`/`create_in_workspace_authorized`).
pub const LOCAL_ACTION_ROOM_CREATE: &str = "room.create";
/// Local audit token produced by room-archive (archived=true) producers.
pub const LOCAL_ACTION_ROOM_ARCHIVE: &str = "room.archived";
```

- **不新增出站契约 token**：message/room 车道 payload `action` = 本地 token 原样（half-B drill 先例 `back.action == "message.deleted"`）；`MODERATION_OUTBOUND_ACTION` 仍是唯一出站契约 token。
- **不新增 envelope 类型**：`AuditClaimPayload::new` 即生产者构建器。
- pin 测试：镜像 `vocabulary_consts_are_pinned` 形态，5 常量规范值断言。

### 2.2 `crates/aero-storage/src/audit_governance.rs`（H3 落点，R4）

模块非测试区新增（`#[cfg(test)] mod db_tests` 之前）：

```rust
/// H3 交付：v2 出站仓储（0239 表）。event_id = audit_events.id（1:1）。
/// 签名与 auth sibling design §2.1 逐字一致——共享落点纪律，全仓只落一次。
impl AuditGovernanceOutboxRepo {
    pub fn new(pool: PgPool) -> Self;

    /// 事务内写 outbox 行。0239 PK 自带 ON CONFLICT (event_id) DO NOTHING → 重放幂等。
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        class: &str,          // GOVERNANCE_CLASS_*（0239 CHECK 值域内）
        priority: i16,        // 10（GOVERNANCE_PRIORITY_BACKLOG，cross-pin 注释）
        payload: serde_json::Value,  // AuditClaimPayload::new 产物（CHECK jsonb_typeof='object'）
    ) -> Result<(), sqlx::Error>;
}

/// 叶子常量 5 臂镜像——storage 侧入队判定（依赖方向：不得 import aero_ai::governance）。
/// 与 aero-ai `governance_lane_for` 由两侧测试双钉：aero-ai per-token pin（§2.3）
/// + 本模块 parity db_tests（§6 AC1）。moderation token 刻意缺席：触发器独占（R5 硬线）。
fn lane_params_for(local_action: &'static str) -> Option<LaneParams> {
    match local_action {
        LOCAL_ACTION_MESSAGE_CREATE | LOCAL_ACTION_MESSAGE_EDIT | LOCAL_ACTION_MESSAGE_DELETE =>
            Some(LaneParams { class: GOVERNANCE_CLASS_MESSAGE, priority: 10, outbound: local_action }),
        LOCAL_ACTION_ROOM_CREATE | LOCAL_ACTION_ROOM_ARCHIVE =>
            Some(LaneParams { class: GOVERNANCE_CLASS_ROOM, priority: 10, outbound: local_action }),
        _ => None,  // unmapped pass-through（R-D2）；message.moderated 不在此列（触发器独占）
    }
}

/// 5 条 message/room 车道 seám 的统一接线点：audit 行 + 条件 outbox 行，同一事务。
/// fail-closed：任一失败 → Err 向上传播 → 调用方事务整体回滚（0 行逃逸）。
/// 供 message.create/edit、room.create/archive 使用（这些路径今天零 audit 行）。
pub async fn append_lane_pair_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    workspace: WorkspaceId,
    actor: Option<ParticipantId>,
    action: &'static str,           // 叶子 LOCAL_ACTION_* 常量
    target: Option<&str>,           // message/room id
    detail: serde_json::Value,      // 最小化（json!({}) 或 delete 既有 detail 形状）
    source_system: &str,            // binding 值或 "aero-im"（R4 回退，见 §2.2.1）
) -> Result<AuditId, sqlx::Error>;  // audit 行必写（调用点全部传映射 token）；outbox 行按 lane_params_for

/// delete 路径专用：audit 行已由既有代码写好（crud.rs:374 / events.rs:337），
/// 只补条件 outbox 行。Ok(true) = 已入队；Ok(false) = unmapped/moderation（不写）。
pub async fn append_outbox_for_audit_in_tx(
    tx: &mut Transaction<'_, Postgres>,
    event_id: AuditId,
    workspace: WorkspaceId,
    action: &'static str,
    source_system: &str,
) -> Result<bool, sqlx::Error>;
```

**2.2.1 envelope 与 source_system 解析**（共享 helper 消费规则）：

- envelope 构建消费 auth sibling §2.1 的 `governance_envelope(event_id, workspace, actor, target, detail, outbound_action, outcome, occurred_at, source_system)`（**签名以 auth design 为准，两切片合并顺序规则见 §7 D3**）：`outbound_action = action 自身`、`outcome = "success"`、`occurred_at = audit 行 created_at`（读回，见 D2）、`source_system = 解析值`。
- source_system 解析 = 每行一次非 RAISE 查询（**不**用触发器侧的 `aero_snaplink_binding_for_workspace`——它缺 binding 时 RAISE，显式写路禁止复制 Gate 2）。**可达性注（finding 1）**：runtime ON 下 D13 库层不变量保证该 workspace 恒有 enabled binding（`configure_enabled` 同事务覆盖检查，`snaplink_commercial.rs:453`）→ lookup 恒命中；fallback `"aero-im"` 仅 runtime OFF（0236 Gate 1 pass-through）时可达——F7/F14）：

```sql
SELECT source_system FROM snaplink_commercial_bindings
 WHERE workspace_id = $1 AND enabled
```
`fetch_optional` → `Some(v)` 用之，`None` 回退 `"aero-im"`（与叶子 sample payload 同源，audit.rs:441）。

### 2.3 `crates/aero-ai/src/governance.rs`（R3，映射权威扩 5 臂）

- `pub use` re-export 链（:41-46）追加 5 个叶子常量。
- `governance_lane_for` 新增 5 臂：

```rust
LOCAL_ACTION_MESSAGE_CREATE | LOCAL_ACTION_MESSAGE_EDIT | LOCAL_ACTION_MESSAGE_DELETE =>
    Some(GovernanceLane { class: GOVERNANCE_CLASS_MESSAGE, priority: GOVERNANCE_PRIORITY_BACKLOG,
                          outbound_action: /* 本地 token 自身 */, status: 0 }),
LOCAL_ACTION_ROOM_CREATE | LOCAL_ACTION_ROOM_ARCHIVE =>
    Some(GovernanceLane { class: GOVERNANCE_CLASS_ROOM, priority: GOVERNANCE_PRIORITY_BACKLOG,
                          outbound_action: /* 本地 token 自身 */, status: 0 }),
```
match 臂内 `outbound_action` 直接用对应常量（`&'static str`）。`is_admin_class` 语义不变（5 新 token 全 false）。

单测换形（保留断言意图）：

| 测试（现 :126/:152/:163/:195） | 换形 |
|---|---|
| `unknown_local_token_passes_through_unmapped` | 负例集缩为 `["call.join", "message.recalled", ""]`（`message.recalled` 兼钉 recall 不在 token 集）；`is_admin_class` 负断言保留 |
| `user_delete_token_stays_out_of_admin_lane` | `assert_eq!(governance_lane_for("message.deleted"), None)` → `Some(lane)` + `lane.class == GOVERNANCE_CLASS_MESSAGE` + `!is_admin_class("message.deleted")`——**R-D2 以更强形式保留**（永不 admin）；doc 更新（原「produce no outbound token」表述不适用：message 车道 outbound = 本地 token） |
| `mapping_is_token_keyed` / `admin_class_rows_never_aggregated` | 断言原样通过（5 token 非 admin-class 保持真）；doc 微调 |
| 新增 `message_and_room_lanes_map_to_backlog` | per-token pin：5 token 各 → 正确 class / priority=10 / outbound=自身 / status=0；DESC 语义注释同 `moderation_lane_preempts_backlog_under_desc_claim`（100 > 10，moderation 仍抢先） |

### 2.4 生产者接线（R5+R6，逐 seam）

统一模式：在既有事务内、`tx.commit()` 之前追加「② audit → ③ outbox」；`append_lane_pair_in_tx` / `append_outbox_for_audit_in_tx` 失败 → `?` 传播 → 整事务回滚。**公共签名一律不变**（调用方零改动——im-core 与 server 都不改，见 §3.8）。

| 本地 token | seam（锚点） | 接线要点 |
|---|---|---|
| `message.create` | `insert_row_in_tx`（crud.rs:68，`pub(crate)` 内核，覆盖 insert/insert_idempotent/insert_outboxed 三调用点） | messages INSERT 后：`SELECT workspace_id FROM rooms WHERE id = $1`（0006 起 NOT NULL，必命中）→ `append_lane_pair_in_tx(tx, ws, Some(sender), LOCAL_ACTION_MESSAGE_CREATE, Some(&id.to_string()), json!({}), source)`。**幂等重放安全**：insert_idempotent/insert_outboxed 的重复 client_message_id 路径在调用内核前返回 Existing——不增行（AC1 重放半部） |
| `message.edit` | ① 裸 `edit`（crud.rs:168）② `edit_outboxed_authorized`（authorization.rs:403） | ① 单语句包事务：begin → `SELECT workspace_id FROM rooms WHERE id=$1` → UPDATE → 仅 `Some(r)` 时 append pair（版本冲突 Err(Conflict) / 缺失 Ok(None) **不写**，无域变更 = 无审计行）→ commit。② 已有 tx：`lock_effective_message_write_access` 的返回值从 `.is_none()` 丢弃改为 `let Some(access) = … else { return Err(…) }` 捕获 `access.workspace`；`edit_locked_outboxed_in_tx` 返回 `Some` 后、commit 前 append pair（actor = 入参 actor；`record_history=false` 的系统编辑——unfurl/transcribe——同样记 audit，token 相同） |
| `message.deleted` | ① `soft_delete_audited`（crud.rs:364）② `soft_delete_locked_outboxed_in_tx`（events.rs:300，覆盖 `soft_delete_outboxed_authorized` 与 `soft_delete_outboxed_system`） | audit 行已 in-tx（crud.rs:374 / events.rs:337），补 `append_outbox_for_audit_in_tx`（`lane_params_for` 判：`message.deleted` → 入队；`message.moderated` → skip 触发器独占；`None` → skip）。已删除/缺失消息（`Ok(false)`/`Ok(None)`）不写（parity 重放半部语义）。**moderation 路径零显式写** |
| `room.create` | ① `create_in_workspace`（room.rs:267）② `create_in_workspace_authorized`（room/governance.rs:108，自持事务，两处独立接线） | 各自 rooms+room_members INSERT 后、commit 前：`append_lane_pair_in_tx(tx, workspace, Some(created_by/creator), LOCAL_ACTION_ROOM_CREATE, Some(&id.to_string()), json!({}), source)`。② 的 `RoomMembershipWriteError` 需 `From<sqlx::Error>`（既有 `?` 链已具备） |
| `room.archived` | ① `set_channel_archived_authorized`（room/governance.rs:396）② 裸 `archive`（room.rs:437） | ① 仅 `archived == true` 写（unarchive 出范围，不写）；workspace 来自 `lock_channel_aggregate` 返回值；actor = caller。② 单语句包事务：begin → `SELECT workspace_id FROM rooms WHERE id=$1` → UPDATE → append pair（actor = `None`，系统侧）→ commit。`RoomRepo::unarchive` 不动 |

### 2.5 `scripts/truth-check-lib.sh`（R2 守卫扩展）

AUDIT-FLAG 块（:278-292）扩为 token 集扫描：对 5 个新 token 逐个 `rg -n -F '"<token>"' crates --glob '*.rs'`，命中须 ∈（`CLAIM_AUDIT_FILE` = `crates/aero-common/src/model/audit.rs`）∪ allowlist（`AUDIT_FLAG_ALLOWLIST` 同形登记的第二站点，若有）；违规计入 exit 码。**清理面**（2026-08-08 全仓扫描，逐站点 verified）：

| 站点 | 处置 |
|---|---|
| `message/crud.rs:378`（soft_delete_audited 的 audit action） | → `LOCAL_ACTION_MESSAGE_DELETE`（生产） |
| `message/authorization.rs:503`（soft_delete_outboxed_authorized 的 audit_action） | → `LOCAL_ACTION_MESSAGE_DELETE`（生产） |
| `message/events.rs:622`（db_test 调用点字面量） | → 常量（守卫扫 .rs 含测试） |
| `audit_governance.rs:511/:534/:585/:841`、`audit.rs:737/:761/:837/:862`（db_test 断言字面量） | → 常量（R7 换形时一并） |
| `aero-ai/governance.rs:128-131/:153-154/:168/:198-199`（单测字面量） | → 常量（R3 换形时一并） |
| `aero-common/src/model/audit.rs:446/:460`（leaf 定义 + sample payload） | 豁免（`CLAIM_AUDIT_FILE`） |
| `message/authorization.rs:368`（`"message.recalled"`） | **不在扫描面**（recall 出范围，非 5 token 之一） |

SQL 侧（0239/0241 的 `'message.moderated'`）不在扫描面（非 .rs）。

## 3. Compatibility constraints

1. **零 DDL、零新迁移**：0239/0240/0241 逐字不动（token 集、CHECK、默认值、索引）；0241 reconciler 零改动（仍 token-keyed `message.moderated`，0241:69；producer 行无条件写故无自愈需求）。本切片**无** `cargo build → migrate` 步骤（§5）。
2. **`message.moderated` 出队路不变**：触发器独占 + Gate 1/2 语义（fail-open runtime 门 + fail-closed binding 门）逐字保持；`moderation_finalize_outbox_parity` / `moderation_finalize_runtime_disabled_commits_1_plus_0` / `ddl_contract_defaults_and_checks` / `duplicate_event_id_is_deduped_by_on_conflict` / `rust_produced_payload_matches_0239_envelope` **名称与字面量不动**（harness 钉位 + 契约 fixture），仅裸字面量换常量（语义不变）。
3. **`AuditGovernanceOutboxRepo` 只落一次**：`append_in_tx` 签名与 auth sibling §2.1 **逐字一致**（`(tx, event_id: AuditId, class: &str, priority: i16, payload: serde_json::Value)`）——两切片不得各自定义。auth 的 `append_pair_in_tx_fail_open`（SAVEPOINT fail-open）是 auth 切片专属，本设计不实现、不调用。
4. **依赖方向**：aero-storage 不得依赖 aero-ai——入队判定用 `lane_params_for`（叶子常量镜像），`governance_lane_for` 与 `GOVERNANCE_PRIORITY_*` 的漂移由两侧测试双钉（§2.2/§2.3）；aero-im-core / aero-server **零改动**（所有接线在 storage 方法内部，公共签名不变）。
5. **R-D2 不变量保留且更强**：`message.deleted` 进 message 车道（priority 10），**永不进 admin 车道**；`call.join`/`message.recalled`/`""` 保持 unmapped pass-through（不 RAISE，T-11 fail-closed 负例）；0241 reconciler 永不把 unmapped 行 fabricate 成 admin 行。
6. **v1 路径不动**：0236 触发器照产 `snaplink_delivery_outbox` 行（audit INSERT 即触发，与 outbox 显式写正交）；runtime-ON + 无 binding 的残余态（经受支持路径不可达，D13 库层不变量，见 F14）下 audit INSERT 触发 0236 Gate 2 RAISE → 整事务 fail-closed 回滚——与 message.delete 今天的行为逐字一致；`non_moderation_action_passes_through_unmapped` 的 v1 断言半部保留（§6）。
7. **既有 pin 保持**：b5-pin 37/37（b5-pin.sh:41 `moderation-priority-drill` 槽与 3 条 PASS 断言零改动）；harness `audit_governance::` + `moderation_finalize_outbox_parity` 条目零改动（新 db_tests 落同模块自动覆盖）；`test-b5-pin-guard.sh` 37/37 绿。
8. **公共 API 无破坏**：`MessageRepo`/`RoomRepo` 全部方法签名不变（`insert_row_in_tx` 内核接线、edit/archive 包事务、`_outboxed_authorized` 捕获 access 均为内部行为）；`AuditRepo::append_in_tx` 签名**不变**（~25 调用点，§7 D2）；`NewMessage`/`NewRoom` 不加字段。
9. **触发顺序语义**：显式写路入队不依赖触发器（audit 行照常触发 0239/0236，但 0239 对非 moderation token pass-through、0236 照产 v1 行）——无触发器顺序假设新增。
10. **范围红线**：不改 `message.recalled`（0238 recall，保持 unmapped）；不改 `room.unarchive`；auth 域 token 归 auth sibling；L1 聚合 / delivery_mode 策略（B5-3）[PROPOSED] 不实现；v1 路径与 `governance_lane_for` fail-closed 化（unmapped 改 RAISE）不做（R-D2 禁第二 abort 路径）。

## 4. Failure modes

| # | 失效 | 机制 | 后果 / 处置 |
|---|---|---|---|
| F1 | audit INSERT FK 违例（workspace 不存在/被删） | `?` 传播，事务丢弃 | 域变更 + audit + outbox **三者俱无**（0 行逃逸）——fail-closed 既有语义（crud.rs:359-363「a message is never silently deleted unaudited」）；AC2 delete 注入点 |
| F2 | outbox payload 非 object（毒 payload） | 0239 `CHECK (jsonb_typeof(payload)='object')` → Err | 整事务回滚；AC2 create/edit/room 注入点（`json!(42)`） |
| F3 | outbox event_id 冲突（重放/双写） | `ON CONFLICT (event_id) DO NOTHING` | 幂等跳过；审计行保留（审计轨迹优先）。显式写路不含 moderation token，故触发器/显式双写不可能由代码产生（硬线 + 审查） |
| F4 | edit 版本冲突 / 消息缺失 | 既有 Err(Conflict)/Ok(None) 分支在 audit 前返回 | 无域变更 → 无 audit/outbox 行（AC1 重放半部） |
| F5 | delete 已删除/缺失消息 | `Ok(false)`/`Ok(None)` 在 audit 前返回 | 无新行（AC1 重放半部） |
| F6 | insert 幂等重放 | 重复 client_message_id 在调用内核前返回 Existing | 无新行（AC1 重放半部） |
| F7 | 无 binding / binding 被禁（**runtime OFF**） | `SELECT … AND enabled` → `fetch_optional` → None | `source_system` 回退 `"aero-im"`，**不 RAISE**——fallback 仅此态可达（0236 Gate 1 pass-through，触发器不介入）；R6 保证面 = 显式写路本身 + 全部经受支持路径可达态 |
| F14 | runtime ON + 无 enabled binding（**残余态，经受支持路径不可达**） | D13 库层不变量：`configure_enabled` 同事务 `require_complete_workspace_coverage`（snaplink_commercial.rs:453，所有 `workspaces` 含 nil 行必须有 enabled binding）+ `reject_omitted_enabled_bindings`（:434）+ boot `from_env` fail-loud（runtime.rs:42-70）——生产写开关仅 configure_enabled/configure_disabled/require_disabled 三 repo 方法（无 HTTP 端点） | 到达时 audit INSERT 触发 0236 Gate 2 RAISE P0001 → 显式写路 `?` 传播 → 整事务回滚（0 行逃逸，fail-closed）——与**今天** message.create（0235 metering 门）/ message.delete（既有 audit 行经 0236）在该态的失败语义**逐字一致**，本设计不新增任何可达态失败；与 connector-root F1 的 auth SAVEPOINT fail-open + DLQ 分类刻意分歧（D4）；AC6 经直接 SQL 翻开关构造此态钉行为（镜像 connector-root `auth_pair_enforcement_on_without_nil_binding_fails_open_drops_pair` 先例） |
| F8 | runtime.enabled=false | 显式写路不 consult runtime | 5 车道行照写（R6 无条件）——与触发器路的 Gate 1 门语义**刻意不同**；0241 reconciler 不动（producer 行无自愈需求） |
| F9 | 高容量 message.create 逐事件一行 | 设计既定（GOVERNANCE_CLASS_MESSAGE doc「High-volume message backlog class (L1-aggregatable)」） | 1:1 行；L1 聚合（[PROPOSED] B5-3）负责后续合并，本设计不实现 |
| F10 | 新车道 claim 排序 | 0240 due 索引 + pg.rs `ORDER BY priority DESC, …` | 新车道 priority 10 严格排在 moderation 100 之后（AC4 钉）；`moderation-in-first-batch` drill 语义不回归 |
| F11 | relay 重放/崩溃 | 既有 connector 状态机（claim/lease/attempts/MAX_ATTEMPTS=5→dead + `ON CONFLICT` + consumer receipt） | 显式写路行同 moderation 行待遇；事件幂等由 event_id 保证 |
| F12 | 进程在事务中途崩溃 | 事务未提交 | 0 行逃逸（同事务原子性）；重试由调用方语义（idempotency key / Ok(false) 等）决定 |
| F13 | edit/裸 archive 包事务的行为回归 | 原单语句原子性由语句自身保证，包事务后相同（版本冲突/竞态语义不变） | audit.rs / audit_governance.rs 既有 db_tests 全绿为回归门（§6） |

## 5. Migration steps

1. **前置（必须先做）**：提交上一批次 untracked B5 基底——`migrations/0239/0240/0241`、`crates/aero-storage/src/audit_governance.rs`、`crates/aero-ai/src/governance.rs`、`crates/aero-audit-connector/`、drills、b5-pin.sh、test-integration.sh 改动（2026-08-08 `git status` 确认全部 `??`——req §8 义务；否则 `git reset --hard` 校准丢地基）。
2. **本切片零迁移**（§3.1）——无 build→migrate 步骤（0236 触发器**零改动**——finding 1 处置 = rescope + AC6 pin 而非触发器改，见 D7；批次迁移编号 0242/0243/0244 分配不受影响）。
3. **R1+R2（叶子）**：5 常量 + pin 测试 → `cargo test -p aero-common` 先绿（基线 112+3/1 不回归）；truth-check 守卫扩展 + 全仓清理面（§2.5 表）→ 5 token 扫描 = 0（AC3）。
4. **R3（映射权威）**：governance.rs 5 臂 + re-export + 单测换形 → `cargo test -p aero-ai governance::`（≥11 项全绿，AC5①）。
5. **R4（仓储）**：`AuditGovernanceOutboxRepo` + `lane_params_for` + 两个 append helper + source_system 解析，落 `audit_governance.rs` 非测试区 → `cargo check --workspace` 干净（与 auth sibling 的合并顺序规则见 §7 D3）。
6. **R5+R6（接线）**：5 条 seam（§2.4 表）→ `cargo test --workspace --lib`（PG 门控 `-- --ignored`）。
7. **R7+R8（db_tests）**：per-token parity ×5（AC1/AC2）+ claim-order（AC4）+ 2 个既有 db_test 换形（§6 AC5②③）→ throwaway 库 `audit_governance::` 全绿；`moderation_finalize_outbox_parity` 钉位零改动。
8. **回滚**：纯代码回退（无 DDL）——已写行继续被 relay 消费（status 机自洽）；无 backfill（5 车道行从部署起才产生，历史操作不回填伪造）。
9. **收尾门禁**：`cargo clippy --workspace --all-targets`（不新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `scripts/test-b5-pin-guard.sh` 37/37 · `scripts/test-integration.sh` B5 段全 PASS（AC4/AC5 终验）。

## 6. Testable acceptance mapping（AC1–AC5 → 具体测试）

全部新 db_tests 在 `crates/aero-storage/src/audit_governance.rs` 的 `db_tests` 模块（H3 落点，既有 `audit_governance::` harness 条目自动覆盖，**零 harness 改动**）。PG 门控 `#[ignore = "requires live Postgres"]` + throwaway 已迁移库。复用既有 fixture：`fixture`/`message_in_workspace`/`enable_enforcement_with_binding`（:124）/`reset_governance_table`/`count_governance_rows`/`governance_rows_for`；新增 `seed_room` fixture 与 `poison`（`json!(42)`）注入辅助。

| 验收 | 测试（名 = 断言面） | 断言形态 |
|---|---|---|
| **AC1** per-token parity ×5 | `message_create_outbox_parity` / `message_edit_outbox_parity` / `message_delete_outbox_parity` / `room_create_outbox_parity` / `room_archive_outbox_parity`（镜像 `moderation_finalize_outbox_parity` 形状） | commit 半部：驱动真实生产者 seam（insert_outboxed / edit_outboxed_authorized / soft_delete_audited / create_in_workspace / set_channel_archived_authorized）→ `audit_events` 恰 1 行（action = 本地 token 原样、actor/target 正确）+ outbox 恰 1 行（status=0、class=message/room、priority=10、`event_id`=audit id 1:1、`payload->>'action'`=本地 token、`payload->>'idempotency_key'`=event_id、`source_system`=binding 值或 `"aero-im"`、`occurred_at`=audit 行 created_at）。重放半部：delete 二次执行 → `Ok(false)` 不增行；insert 幂等路径（同 client_message_id）→ Existing 不增行 |
| **AC2** same-tx proof | 每个 parity 测试的**回滚半部**（同函数内第二段） | `message.deleted`：`soft_delete_audited(id, 不存在 WorkspaceId::new(), …)` → Err（FK 违例）→ 消息 `deleted_at` 仍 NULL + 0 audit + 0 outbox 行。create/edit/room：测试自开事务按 seam 的 in-tx 语句序列（域语句 + audit append + outbox append）执行，末步 `append_in_tx(…, json!(42))`（违 0239 `jsonb_typeof` CHECK）→ Err → 事务丢弃后断言：域表 0 行（create/room）/ 域行未变（edit：blocks/version 不变）+ `audit_events` 0 行 + `audit_governance_outbox` 0 行（ws-scoped） |
| **AC3** token 单源 | `scripts/truth-check.sh` 0 违规 + 手工 `rg`（§2.5） | 5 token 在 `audit.rs` 之外返回 0（守卫扩展后自动扫）；SQL 侧不在扫描面 |
| **AC4** claim 排序 | `message_room_lanes_claim_after_moderation_desc`（新 db_test） | 种子：5 行新车道（message×3/room×2，priority 10，`available_at` 更早）+ 1 行 moderation（admin/100，`available_at` 更晚，backlog 先种）；以 **pg.rs `claim_due` 逐字镜像 SQL**（ORDER BY `priority DESC, available_at, created_at, event_id`，cross-pin 注释「must equal crates/aero-audit-connector/src/pg.rs:117」）执行：`LIMIT 1` 首领 = moderation 行；`LIMIT 5` 领走全部 5 行（集合相等）——**新 backlog 车道严格排在 moderation 之后（DESC）**。`scripts/test-b5-pin-guard.sh` 37/37 绿（`moderation-priority-drill` 槽零改动） |
| **AC5** T-11 fail-closed / R-D2 | ① 单测：`governance_lane_for("call.join")==None` / `("message.recalled")==None` / `is_admin_class` 对 5 新 token 全 false（R3 换形后保留）② db_test：直接 `INSERT INTO audit_events`（不经生产者 seam，镜像 `duplicate_event_id_is_deduped_by_on_conflict` 直插形态）+ unmapped token（`'call.join'`）→ 0 outbox 行 + v1 行照产（`non_moderation_action_passes_through_unmapped` 重写后）③ reconciler：`governance_reconcile_backfills_disabled_window` 重写后按行断言（见下） | R7 换形（**非回归，语义更新**）：② 从驱动 `soft_delete_audited`（接线后产 1 行 message 行）改为直插 unmapped token——纯 trigger pass-through 断言，与生产者路径解耦；③ id2 的 `soft_delete_audited` 现产 1 行 message-class producer 行（无条件，R6）→ `governance_rows_for == 2`（1 admin/100 backfill + 1 message/10 producer）+ `fetch_all` 按 `payload->>'action'`/class 逐行断言（admin 行 action=`admin.content.flag`、producer 行 action=`message.deleted`）——「unmapped 永不 fabricate admin 行」断言保留且更强；「dead 不复活」半部不变。`moderation_finalize_outbox_parity` / `ddl_contract_defaults_and_checks` / `moderation_finalize_runtime_disabled_commits_1_plus_0` / `duplicate_event_id_is_deduped_by_on_conflict` / `rust_produced_payload_matches_0239_envelope` **名称与字面量不动**；`audit.rs` 既有 db_tests（:737/:761/:837/:862）字面量换常量，断言语义不变 |
| **AC6** runtime-ON/no-binding 残余态（audit_integrity finding 1 pin） | `enforcement_on_without_binding_message_room_lanes_fail_closed`（新 db_test） | 直接 SQL 翻 `snaplink_commercial_runtime.enabled=TRUE`（绕开 `configure_enabled` 覆盖检查——该态经受支持路径不可达，D13；镜像 connector-root F1 回归先例），**不种 binding**：① **edit seam**——先 runtime OFF 下 `message_in_workspace` 建消息，翻开关后驱动 `edit_outboxed_authorized` → Err（0236 P0001 `snaplink_binding_required`）→ 消息 blocks/version 未变 + audit 0 行 + outbox 0 行（fail-closed，0 行逃逸）；② **room.create seam**——翻开关后驱动 `create_in_workspace_authorized` → Err → rooms 0 行 + audit 0 行 + outbox 0 行；③ **delete seam（既有语义回归 pin）**——翻开关后 `soft_delete_audited` → Err → `deleted_at` 仍 NULL + 0 行逃逸（今天即如此，接线不改变）；④ **create seam（既有语义回归 pin）**——翻开关后 messages INSERT → Err（0235 metering 门，先于 audit 写）——本设计在该态不新增失败面；⑤ **fallback 可达半部**：runtime OFF 下 edit seam 成功 → outbox 行 `source_system="aero-im"`（F7 fallback 证明）。结束 `restore_enforcement_disabled`（既有 fixture）。moderation 0239 Gate 2 abort 由 `moderation_finalize_outbox_parity` 回滚半部照旧钉（不受影响） |

**门禁收口**：`cargo test --workspace --lib`（PG 门控 `-- --ignored` + 一次性库）· clippy 无新增警告 · truth-check/file-size/web-check 0 违规 · `test-b5-pin-guard.sh` 37/37 · `test-integration.sh` B5 段（`audit_governance::` + `moderation_finalize_outbox_parity` 两条目，零 harness 改动）。

## 7. Decision points / risks

- **D1（入队判定归属，req 字面修正）**：req R5 的分支判据「`governance_lane_for(audit_action)`」在 aero-storage 不可达（依赖方向）——钉定为 `lane_params_for`（叶子常量 5 臂镜像），与 aero-ai 映射由两侧测试双钉（§2.2/§2.3）。语义等价：5 新 token 入队、moderation 触发器独占、unmapped pass-through。
- **D2（`AuditRepo::append_in_tx` 签名不变）**：envelope `occurred_at` 需 = audit 行 created_at（触发器用 `NEW.created_at`），但 append_in_tx 内部生成 created_at 且 ~25 个调用点——**不改签名**；helper 在 audit INSERT 后 `SELECT created_at FROM audit_events WHERE id = $1`（同事务 PK 读回，必然可见、精确）。备选（返回 `(AuditId, OffsetDateTime)`）因 25 调用点编译面拒绝。
- **D3（`governance_envelope` 共享合并顺序）**：envelope 构建器签名以 auth sibling §2.1 为准（`outcome`/`occurred_at`/`source_system` 参数化）；两切片**先落地者添加、后落地者消费**，签名逐字一致（同 §3.3 纪律）。若 auth 切片延期，本切片按同一签名自备（避免双份）。
- **D4（fail-closed vs auth fail-open 分歧，req 维持）**：message/room 车道同事务 fail-closed（审计失败 = 域操作失败，0 行逃逸；ROADMAP 方向五先例 + moderation Gate 2 语义）；auth sibling 的 SAVEPOINT fail-open 是 auth 切片的 UX 决策，不适用本车道。风险：message send 因 audit 写失败而失败——仅 FK/CHECK 违例（bug 级），F1/F2 注入即证明路径。
- **D5（`message.deleted` 语义后果）**：v1 路径（0236）与 0241 reconciler 均 token-keyed 于各自 SQL 字面量，不受 Rust 映射扩展影响（SQL 零改动）；换形面 = R7 两测试（§6 AC5②③），已逐条枚举，无隐藏面。
- **D6（高容量 message.create）**：1:1 逐事件行是既定形态（L1 聚合 [PROPOSED] 负责合并）；行数增长与消息量线性——既有 outbox 表与 claim 机（0240 索引）已按此设计。
- **D7（0236 v1-trigger RAISE 处置，audit_integrity finding 1——机制定稿 = F7/R6 rescope + AC6 pin）**：候选三机制逐条裁决——**触发器改（0236 Gate 2 改 fail-open skip）拒绝**：移除 v1 商业 canary = 弱化 snaplink 触发器路径（gate 判据 2），且与批次共享分析（connector-root §7 D13/F14：`configure_enabled` 覆盖检查与开关翻转同事务 + boot fail-loud，runtime-ON ⇒ 每 workspace 有 enabled binding 为库层强制）矛盾——该态经受支持路径不可达，触发器 RAISE 是该残余面的既有 fail-closed 语义（今天 message.create 经 0235 metering、message.delete 经既有 audit 行即如此失败），改之则全系统 audit 生产者（~24 调用点）在该态从 fail-loud 变静默丢 v1 行；**seeded binding（测试全种 binding）拒绝**：即 finding 所指 status quo，状态留 CI 盲区；**F7/R6 rescope + AC6 采纳**：R6 保证面精确化为「显式写路本身 + 全部经受支持路径可达态」——可达态下发送永不因缺 binding 失败（D13 保证 binding 恒在），残余态失败语义与今天逐字一致；零新迁移、零触发器改动（保留 §3.1 零 DDL 承诺与 0242/0243/0244 编号分配），AC6 把该态从盲区变钉死行为。
- **Risks**：① 前置批次 untracked 未提交则校准丢地基（§5.1 硬前置，2026-08-08 实测确认）；② edit/裸 archive 包事务行为面 = 版本冲突/竞态语义不变，但 audit.rs/audit_governance.rs 既有 db_tests 必须全绿（F13）；③ truth-check 守卫扩到 token 集可能误伤既有测试字面量——清理面（§2.5 表）已全量枚举并 verified，无未列站点；④ `create_in_workspace_authorized` 的 `RoomMembershipWriteError` 需承载 sqlx 错误（既有 `?` 链已具备，编译期验证）；⑤ 与 auth sibling 的合并顺序（D3）若失序 → 同一模块双份 helper 冲突——以签名钉 + 先落地者添加规则规避。

## 8. Sequencing

1. **前置**：提交上一批次 untracked B5 基底（0239/0240/0241、`audit_governance.rs`、`governance.rs`、`aero-audit-connector/`、drills、b5-pin.sh、test-integration.sh）——AC3/AC4 基线前提。
2. R1+R2：叶子 5 token + pin 测试 + truth-check 守卫扩展 + 全仓字面量清理 → `cargo test -p aero-common` 先绿；5 token 扫描 = 0（AC3）。
3. R3：governance.rs 5 臂 + 单测换形 → `cargo test -p aero-ai governance::`（AC5①）。
4. R4：`AuditGovernanceOutboxRepo` + helpers 落 `audit_governance.rs` → `cargo check --workspace` 干净（D3 合并顺序规则）。
5. R5+R6：5 条生产者 seam 接线 → `cargo test --workspace --lib`（PG 门控 `-- --ignored`）。
6. R7+R8：per-token parity ×5（AC1/AC2）+ claim-order（AC4）+ 2 个既有 db_test 换形（AC5②③）→ throwaway 库 `audit_governance::` 全绿；`moderation_finalize_outbox_parity` 钉位零改动。
7. 收尾门禁：clippy · truth-check/file-size/web-check · `test-b5-pin-guard.sh` 37/37 · `test-integration.sh` B5 段全 PASS（AC4/AC5 终验）。
