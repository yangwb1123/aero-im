# Design — aero-im-core B5-1：ImService 写路径同事务审计 + 治理 outbox 入队 seam（producer side，handoff H3）

- **Direction**: "In-tx audit + governance-outbox enqueue seam for ImService message.*/room.*/admin.* operations (B5-1 producer side, handoff H3)"
- **Module (analysis root)**: `crates/aero-im-core/src`（ImService 写路径 = 生产者契约面）；交付面含 `crates/aero-storage`（tx seam 接线 + H3 落点）、`crates/aero-ai/src/governance.rs`（lane 映射权威）、`crates/aero-common/src/model/audit.rs`（叶子 token 词表）
- **Requirements**: `docs/requirements/2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.req.md`
- **Campaign**: `aero-im-b5-outbox-relay`；in-repo contract anchor `docs/proposals/audit-contract-batch-aero-im.md`
- **Sibling specs（边界协调）**: `2026-08-08-aero-common-b5-1-lane-mapping-in-tx-outbox.req.md`（共享基础设施腿，未落地）、`2026-08-08-aero-im-core-b5-1-in-tx-audit-governance-enqueue.req.md`（cfe64e80 更宽 token 集，未落地）、`2026-08-08-aero-ai-b5-1-migration-0239-governance-outbox.req.md`（0239 落地，已完成）
- **Status**: Design（证据核验完成 2026-08-08；**F-1/F-2 裁决并入 §8（2026-08-08）**；行号为核对时锚点，可能漂移——**文件/符号**才是稳定锚点）

---

## 0. 证据核验（evidence = untrusted claims，逐条对源码）

所有符号锚点**全部命中**；行号仅有已登记的微漂。逐条结论：

| 证据 | 结论 | 核验要点 |
|---|---|---|
| E1 messages.rs 三函数 + ImService 零审计 | ✅ | `send_message_inner` :139（`insert_outboxed` 调用点 **:216**）；`delete_message` :430（`soft_delete_outboxed_authorized` :447）；`moderate_delete` :611（workspace=None 拒绝 :619-627；`soft_delete_outboxed_system(…, Some("message.moderated"), …)` :645）。全文件 `AuditRepo`/`audit_events` 引用 = 0（仅 :533/:619-627 R-D1 注释） |
| E2 audit_governance.rs:22 H3 手记、1141 行、repo 不存在 | ✅ | 模块 doc :22-24 原文 "add the repo to this same module when it lands"；`rg AuditGovernanceOutboxRepo crates/` 唯一命中 = 该 doc。**1141 行**（1141+59=1200 HARD，精确） |
| E3 `soft_delete_locked_outboxed_in_tx` :293-355，:337 append 丢弃 AuditId | ✅ | `soft_delete_outboxed_system` :267-292（自有 begin/commit）；:337 条件 `AuditRepo::append_in_tx(...).await?;`——**裸语句，返回值被丢弃**；`RoomEvent::Deleted` event outbox 同 tx 尾随 |
| E4 `AuditRepo::append_in_tx` :127 返回 AuditId | ✅ | :127-136，`-> Result<AuditId, sqlx::Error>`；`append_on` 共享 INSERT :140 |
| E5 0239 trigger token-keyed、fail-open pass、binding RAISE | ✅ | Gate 1 runtime 关 → RETURN NEW；token 门 `NEW.action <> 'message.moderated'` → RETURN NEW（零 RAISE）；Gate 2 `aero_snaplink_binding_for_workspace` RAISE；INSERT class 'admin'/priority 100 + `ON CONFLICT (event_id) DO NOTHING`。DDL CHECKs：`class IN ('admin','message','room')`、`priority > 0`、`delivery_mode IN ('push')`、`status IN (0,1,2,3)`、`jsonb_typeof(payload)='object'`。**envelope = 16 键**（event_id/source_system/event_type/schema_id/schema_version/occurred_at/actor/targets/aggregate_type/aggregate_id/action/outcome/payload/data_classification/retention_class/idempotency_key） |
| E6 `governance_lane_for` 单臂、`message.deleted` 钉 None | ✅ | :74-102 唯一臂 `LOCAL_ACTION_MODERATED` → `{admin,100,MODERATION_OUTBOUND_ACTION,0}`，`_ => None`；`GOVERNANCE_PRIORITY_{MODERATION=100,BACKLOG=10}` :31/:33；re-export 链 :41-47。`unknown_local_token_passes_through_unmapped` 负例集含 `message.deleted`；`user_delete_token_stays_out_of_admin_lane` 钉 None（R-D2）——均与验收换形一致 |
| E7 `insert_outboxed` 零审计 | ✅ | idempotency.rs:158-236：幂等早退（`find_outboxed_by_client_message_id`，**tx 之前**）→ tx 内 lock（:182，`LockedRoomWriteAccess{workspace}` 被折叠成 bool **丢弃**）→ insert_row → event outbox → side-effect → message_send_keys claim → commit。零 audit 引用 |
| E8 edit 零审计、recall 已审计 | ✅ | `edit_outboxed_authorized` authorization.rs:399-453：`lock_effective_message_write_access(...).await?.is_none()` ——**workspace 已解析但被丢弃**；`edit_locked_outboxed_in_tx` events.rs:69-140 零 audit。`recall_locked_outboxed_in_tx` authorization.rs:292-374：:364-372 in-tx 写 `"message.recalled"`（detail=`{room_id,digest}`），AuditId 丢弃 |
| E9 room 写路径零审计 | ✅ | `create_in_workspace_authorized` room/governance.rs:108-153（workspace = 入参；room INSERT + owner member INSERT；commit 在尾部）；`add_member_authorized` :173-218（`lock_room_aggregate` 解析 workspace；`rows_affected()==1` 产 `inserted` bool）；`set_channel_archived_authorized` :396；`RoomRepo::create_in_workspace` room.rs:267（legacy 裸路径）。im-core `create_room_in_workspace` room.rs:63→:77、`add_member` :266→:274、`archive_channel` channels.rs:123→:130。room 侧全零 audit |
| E10 harness 槽位 + 负例名不存在 | ✅ | test-integration.sh:318-325 两条 `run_migrated_integration`（`audit_governance::` / `moderation_finalize_outbox_parity`），空过滤守卫 `test result: ok. [1-9][0-9]* passed`（:206）；b5-pin.sh B5_CONTRACT_TEST_LIST 含全部钉名。`moderation_finalize_without_binding_aborts_tx` **仅存于 docs**；落库镜像目标 = `moderation_finalize_outbox_parity`（:247）内 Rollback half（:353）。6 个 db_tests 全在：parity :247 / ddl_contract :624 / runtime_disabled :782 / non_moderation :827 / duplicate :892 / reconcile :976。priority drill `moderation-in-first-batch: PASS` :276 |
| 更正① delete 已审计、增量 = outbox 行 | ✅ | 用户删除走 `soft_delete_outboxed_authorized`（authorization.rs:454-507，`Some("message.deleted")` :499）→ `soft_delete_locked_outboxed_in_tx` :337 写 audit 行。**缺的只是 outbox 行** |
| 更正② recall 已审计且出范围 | ✅ | 见 E8；`message.recalled` 不在五 token 验收集 |
| 更正③ 负例测试名 | ✅ | 见 E10；另发现仓库**已有同语义先例**：`audit_failure_rolls_back_delete_and_outbox_append`（events.rs:607-647）——bogus workspace → audit FK Err → 软删回滚 + 0 audit 行，是验收 (d)「unknown workspace aborts」的直接镜像对象 |
| R7 依赖面（额外核验） | ✅ | `soft_delete_audited`/`soft_delete_moderated`（crud.rs:369/:398）走**独立** `soft_delete_in_tx`，**不经过** `soft_delete_locked_outboxed_in_tx`——delete seam 不触碰其零 outbox 语义；`rust_produced_payload_matches_0239_envelope` :450 存在；half-B :544-548 四列 INSERT + :577-605 逐字段断言（`back.action == "message.deleted"` 本地 token 原样先例）；`AuditClaimPayload`/`AuditActor`/`AuditTarget` 在 aero-common 叶子，aero-storage 可用（half-B 已用） |

**核验中发现的额外设计输入**（证据未覆盖、本设计采纳）：
1. `lock_effective_message_write_access` 返回 `LockedRoomWriteAccess { workspace }`（authorization.rs:21/:99-151，room 行 `FOR SHARE` 锁在 tx 内）——edit seam 可直接捕获；send seam 的 room 行已被同锁，后续 `SELECT workspace_id FROM rooms` 无竞态。
2. `edit_outboxed_authorized` 现把 workspace 丢弃（`.is_none()`），capture 是 3 行内的小重构。
3. 0239 trigger envelope 的 `source_system` 来自 binding（Gate 2 保证 trigger 路必有 binding）；**Rust 显式写路无 binding 也写行**（无条件入队），`source_system` 需 binding 查询 + `"aero-im"` 回退（sibling aero-common R4 同款）。
4. truth-check AUDIT-FLAG 守卫机制（truth-check-lib.sh:278-291）：`rg -n -F` 扫描 + `path:line` allowlist + 过期条目 drift 检查——R1 的 5-token 扫描照此扩展。

---

## 1. API 变更

### 1.1 叶子 token 词表（`aero-common/src/model/audit.rs`，action-token 区 :156 后）

```rust
pub const LOCAL_ACTION_MESSAGE_SEND: &str = "message.send";      // class: message
pub const LOCAL_ACTION_MESSAGE_EDITED: &str = "message.edited";  // class: message
pub const LOCAL_ACTION_MESSAGE_DELETE: &str = "message.deleted"; // class: message
pub const LOCAL_ACTION_ROOM_MEMBER_ADD: &str = "room.member.add";// class: room
pub const LOCAL_ACTION_ROOM_CREATED: &str = "room.created";      // class: room
```

- `message.deleted` 是既有生产字面量（crud.rs:378 / authorization.rs:499）——**常量化后生产点换常量，不是新语义**；`LOCAL_ACTION_MODERATED`（:156）不动。
- **token 集 = 5 不变**（F-1/F-2 裁决，§8）：系统编辑（unfurl/transcribe）复用 `message.edited`、DM/group-DM 创建复用 `room.created`——actor（`None` vs 用户）与 `detail.kind` 区分，零新叶子常量、零新 lane 臂、truth-check 扫描面不变。
- **truth-check 守卫扩展**（truth-check-lib.sh 新 scan block，照 AUDIT-FLAG :278-291 机制）：5 token 逐个 `rg -n -F`，命中须 ∈ 叶子文件（豁免）或 `path:line` allowlist。生产字面量点（crud.rs:378、authorization.rs:499）**换常量**；钉位测试字面量（half-B :534/:588、events.rs:622、governance.rs 单测）进 allowlist（stale-entry drift 检查照旧，防 allowlist 静默掩盖漂移）。
- payload `action` = 本地 token 原样（half-B 先例 `back.action == "message.deleted"`；v1 0236 路径同）；不新增出站契约 token（`MODERATION_OUTBOUND_ACTION` 仍唯一出站 token）。

### 1.2 lane 映射扩 5 臂（`aero-ai/src/governance.rs` `governance_lane_for` :74-102）

```rust
LOCAL_ACTION_MESSAGE_SEND | LOCAL_ACTION_MESSAGE_EDITED | LOCAL_ACTION_MESSAGE_DELETE => Some(GovernanceLane {
    class: GOVERNANCE_CLASS_MESSAGE, priority: GOVERNANCE_PRIORITY_BACKLOG,
    outbound_action: <本地 token 自身>, status: 0,
}),
LOCAL_ACTION_ROOM_MEMBER_ADD | LOCAL_ACTION_ROOM_CREATED => Some(GovernanceLane {
    class: GOVERNANCE_CLASS_ROOM, priority: GOVERNANCE_PRIORITY_BACKLOG,
    outbound_action: <本地 token 自身>, status: 0,
}),
```

- `outbound_action` = 本地 token 自身（payload 原样）；**admin 车道保持仅 `message.moderated`**（`is_admin_class` 对 5 新 token 全 false——`mapping_is_token_keyed` / `admin_class_rows_never_aggregated` 的 `!is_admin_class` 循环逐字保持）。
- 单测换形（仅两处）：`unknown_local_token_passes_through_unmapped` 负例集移除 `message.deleted`（其余 `room.create`/`message.create`/`message.edit`/`call.join`/`""` 仍 None）；`user_delete_token_stays_out_of_admin_lane` 改 `Some(lane)` + `lane.class == GOVERNANCE_CLASS_MESSAGE` + `!is_admin_class`——**R-D2「永不 admin」以更强形式保留**。新增 per-token pin 测试（5 token → class/priority=10/outbound=自身/status=0）。

### 1.3 `AuditGovernanceOutboxRepo`（H3，落 `crates/aero-storage/src/audit_governance/`）

**尺寸强制拆模块**（file-size-check 唯一权威；现文件 1141 行，repo ~40 行 + 新测试 ~250 行必超 1200 HARD）：

```
crates/aero-storage/src/audit_governance.rs  →  audit_governance/mod.rs（模块 doc + repo）
crates/aero-storage/src/audit_governance/
    ├── mod.rs           模块 doc（含 H3 手记原文）+ AuditGovernanceOutboxRepo（§1.3）
    ├── db_tests.rs      既有 fixtures（逐字搬移，零改动；harness 过滤 `audit_governance::` 子串命中不变）
    └── producer_tests.rs 新五 lane parity 测试（§5）
```

```rust
pub struct AuditGovernanceOutboxRepo { pool: PgPool }

impl AuditGovernanceOutboxRepo {
    pub fn new(pool: PgPool) -> Self;

    /// 0239 表的事务内写行（event_id = audit_events.id，1:1）。
    /// INSERT (event_id, class, priority, payload) … ON CONFLICT (event_id) DO NOTHING
    /// （重放/未来 trigger 双写幂等——duplicate_event_id_is_deduped_by_on_conflict 语义）。
    /// status/available_at/created_at/attempts 走 DDL 默认（0/clock_timestamp()/0），
    /// T-11 claim 谓词兼容。镜像 half-B :544-548 四列形状。
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        class: &str,          // GOVERNANCE_CLASS_*（0239 CHECK 值域）
        priority: i16,         // 10 = GOVERNANCE_PRIORITY_BACKLOG（comment-pin，aero-storage 不得 import aero-ai）
        payload: serde_json::Value, // 16-key envelope（jsonb object CHECK）
    ) -> Result<(), sqlx::Error>;

    /// 构建 16-key 0239 envelope（镜像 trigger jsonb_build_object + half-B
    /// AuditClaimPayload::new 断言集 :577-605）：
    ///   * occurred_at：`SELECT to_jsonb(created_at) #>> '{}' FROM audit_events WHERE id=$1`
    ///     （**绝不** Rust formatter；half-B :522-527 先例）；
    ///   * source_system：`SELECT source_system FROM snaplink_commercial_bindings
    ///     WHERE workspace_id=$1` → Option → 回退常量 "aero-im"（叶子 sample_payload 同源）——
    ///     无 binding 不 raise、不入商业门（与 trigger Gate 2 刻意不对称，§3）；
    ///   * actor = AuditActor::participant / system；targets = [AuditTarget::resource(target)]；
    ///     aggregate_type='workspace'、aggregate_id=workspace、action=本地 token 原样、
    ///     outcome='success'、data_classification='confidential'、retention_class='security'、
    ///     idempotency_key=event_id。
    pub async fn envelope_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        workspace: WorkspaceId,
        actor: Option<ParticipantId>,
        target: Option<&str>,
        action: &str,
        detail: serde_json::Value,
    ) -> Result<serde_json::Value, sqlx::Error>;
}
```

- **门语义 = 无条件入队**：不 consult `snaplink_commercial_runtime.enabled`、无 binding RAISE——与 0239 trigger Gate 1/Gate 2 是**刻意不对称**（trigger 门属 moderation SQL 路专属；app 侧交付契约 = 行必落 status 0，投递门在 connector）。0241 reconciler 零改动（Rust 路行在写时已落）。
- `lib.rs` 的 `pub mod audit_governance;` 不变；`AuditGovernanceOutboxRepo` 经 `pub use`（照 `AuditRepo` 惯例，无 token-helper 撞名风险）。

### 1.4 producer seam 接线（五条 + F-1/F-2 裁决扩展三条 = S-1/S-2/S-3，全部 in-tx，ImService 零改动）

统一模式：① 域变更 → ② `AuditRepo::append_in_tx`（**捕获 AuditId**）→ ③ `envelope_in_tx` + `append_in_tx`。任一失败 → 整事务回滚（0 行逃逸）。

| token | seam 位置 | 接线要点（具体到语句） |
|---|---|---|
| `message.send` | `insert_outboxed`（idempotency.rs:158，side-effect INSERT 后、commit 前） | `SELECT workspace_id FROM rooms WHERE id=$1` in-tx（room 行已被 `lock_effective_sender_room_access` FOR SHARE 锁，无竞态；失败 = Err = fail-closed）；actor = sender；target = message id；detail = `{"room_id"}`。幂等重放早退（:166-176，tx 之前）不写（无域变更） |
| `message.edited` | `edit_outboxed_authorized`（authorization.rs:399，`edit_locked_outboxed_in_tx` 返 `Ok(Some)` 后、commit 前） | 把 `lock_effective_message_write_access(...).await?.is_none()` 改为捕获 `Some(access)`（`LockedRoomWriteAccess.workspace`，3 行内重构）；actor = actor；target = message id；detail = `{"room_id", "digest": <编辑后 blocks 的稳定内容摘要>}`（recall `{room_id,digest}` 惯例，authorization.rs:368-372）。`Ok(None)`（version 冲突/已删/已 recall）不写。系统编辑 seam 见 S-1/S-2（§8.1，不再零改动） |
| `message.deleted` | `soft_delete_locked_outboxed_in_tx`（events.rs:293，:337 条件 append 处） | 捕获 AuditId；**仅当 `action == LOCAL_ACTION_MESSAGE_DELETE` 时补 outbox 行**（class message/10）；`message.moderated`（`soft_delete_outboxed_system` moderation 路）**跳过**——trigger 是该 token 唯一生产者（不双写，语义硬线）；`(workspace, action)` 任一 None 不写（无审计分支原样）。`soft_delete_audited`/`soft_delete_moderated`（crud.rs，测试专用，独立 `soft_delete_in_tx`）**零改动** |
| `room.created` | `create_in_workspace_authorized`（room/governance.rs:108，owner member INSERT 后、commit 前） | workspace = 入参；actor = creator；target = room id；detail = `{"room_id", "kind", "name"}`。`RoomRepo::create_in_workspace`（room.rs:267 legacy 裸路径）零改动 |
| `room.member.add` | `add_member_authorized`（room/governance.rs:173，**仅 `inserted == true`**、commit 前） | workspace 来自 `lock_room_aggregate`；actor = caller；target = room id；detail = `{"room_id", "member"}`。已存在成员（`Ok(false)`）不写 |
| **S-1** `message.edited`（系统：unfurl） | `edit_outboxed_system`（events.rs:42，包装函数：`edit_locked_outboxed_in_tx` 返 `Ok(Some)` 后、commit 前） | **F-1 裁决：接线**（§8.1）。tx 内已 `lock_message_in_tx`（FOR UPDATE）→ `SELECT workspace_id FROM rooms WHERE id=$1`（无竞态；rooms.workspace_id 写一次后不可变）→ `AuditRepo::append_in_tx`（**actor=None**，`audit_events.actor_id` 可空 0007）+ `envelope_in_tx` + outbox append（token `message.edited` 复用）；detail = `{"room_id", "kind":"unfurl"}`。`Ok(None)`/版本冲突不写；失败 = Err 回滚 → unfurl_bot 既有 Fail-open log-skip（消息不丢，仅卡不附） |
| **S-2** `message.edited`（系统：转写） | `update_voice_transcript_outboxed`（events.rs:~165，event outbox 后、commit 前） | **F-1 裁决：接线**（§8.1）——**设计此前误标为 `edit_outboxed_system`，实为独立函数，本裁决修正命名**。同 S-1 模式；detail = `{"room_id", "kind":"voice_transcript"}`；`changed=false`（无未转写 Voice 块）→ `Ok(None)` 不写 |
| **S-3** `room.created`（DM 创建） | `DmRepo::find_or_create_in_workspace`（dm.rs:104，member INSERT 后、commit 前）与 `GroupDmRepo::find_or_create_in_workspace`（group_dm.rs:164，is_group_dm mark 后、commit 前） | **F-2 裁决：接线**（§8.2）。workspace = 函数入参（生产传 `DEFAULT_WORKSPACE_ID`，无需解析）；**仅创建分支**写一条 `room.created`（与频道创建单行语义对齐——初始名册入 detail 而非逐边 member.add）：actor = 发起者（1:1 的 `a` / group 的 `creator`）；detail = `{"room_id", "kind":"direct"|"group", "members":[...]}`。existing 早退（find 命中）不写。`create_in_workspace_authorized` 拒绝 `RoomKind::Direct`（FixedMembership）——此 seam 是 DM 类房间的唯一审计入口 |

- **fail-closed 规则**：seam 内 workspace 解析失败 / audit 写失败 / enqueue 写失败 = `Err`（整事务回滚），**绝不静默 skip**——验收 (d) 的服务端实现。
- **ImService 层（`crates/aero-im-core/src/service/`）零改动**：调用形状冻结，seam 全在 storage tx 内。

---

## 2. 兼容性约束

| 面 | 约束 | 机制 |
|---|---|---|
| **DDL/迁移** | 零迁移（0239/0240/0241 一字不改） | 本 direction 无 `migrations/NNNN_*.sql`；trigger 仍是 `message.moderated` 唯一 SQL 生产者 |
| **connector/relay/drills** | 零改动（aero-audit-connector、aero-eng、main.rs boot） | 新行形状 = DDL 默认 + T-11 谓词兼容；claim 排序零改动（priority 10 ≤ 100 不扰动 moderation 优先） |
| **harness** | 零改动（无新 37-slot、无新 integration 段） | 新测试在 `audit_governance::producer_tests` 模块 + `moderation_finalize_outbox_parity_` 前缀 → 两条目空过滤守卫均非空执行 |
| **ImService API** | 调用形状冻结 | seam 全在 storage tx 内；无签名变更、无新路由、无新 WS 帧 |
| **crate 依赖方向** | aero-storage 不得依赖 aero-ai | class 用叶子 `GOVERNANCE_CLASS_*`；priority 用字面量 10 + comment-pin（half-B :543-547 既有惯例） |
| **钉位测试** | 逐字保持：`moderation_finalize_outbox_parity`、`moderation_finalize_runtime_disabled_commits_1_plus_0`、`non_moderation_action_passes_through_unmapped`（走 `soft_delete_audited`——独立路径，零 outbox 语义不变）、`governance_reconcile_backfills_disabled_window`、`duplicate_event_id_is_deduped_by_on_conflict`、`ddl_contract_defaults_and_checks`、`rust_produced_payload_matches_0239_envelope` | ① delete seam 条件化（仅 `message.deleted` 写）；② `soft_delete_audited` 不经过 seam；③ parity 测试名/字面量不动 |
| **v1 共存** | 0236 `snaplink_delivery_outbox` 不重定向 | 新 audit 行照常产 v1 行（unmapped token → v1 payload 本地 token 原样，half-B "v1 keeps the local token" 语义对五 lane 成立） |
| **出范围 token** | `message.recalled`/pin/archive_channel/`message.react`/join/leave/role/ownership 等保持 unmapped | R-D2/T-11 负例语义不变；**完整排除矩阵见 §8.3**（F-1/F-2 裁决后：系统编辑与 DM 创建已接线，不再属排除面） |
| **governance.rs 其它单测** | `mapping_is_token_keyed`/`admin_class_rows_never_aggregated`/`outbound_action_is_single_contract_token`/`moderation_lane_preempts_backlog_under_desc_claim` 逐字保持 | 5 新 token 全部非 admin-class，`!is_admin_class` 循环不红 |

---

## 3. 失败模式

| # | 场景 | 行为 | 与既有语义的关系 |
|---|---|---|---|
| F1 | seam 内 workspace 解析失败（room 行缺失/被删） | `Err` → 整事务回滚（域行 0 + audit 0 + outbox 0） | 镜像 `audit_failure_rolls_back_delete_and_outbox_append`（events.rs:607-647 bogus-workspace 先例）；验收 (d) |
| F2 | audit INSERT 失败（FK/约束） | `Err` → 回滚 | 与现 delete 语义同（crud.rs:359-363「a message is never silently deleted unaudited」） |
| F3 | outbox INSERT 失败（CHECK 违反，如 class 拼错） | `Err` → 回滚 | 0239 value CHECK 的 fail-closed 面；class 一律走叶子常量，拼错在编译期不可达 |
| F4 | enforcement ON + 无 binding 的 workspace 上 send/edit/delete/room 写 | 0236 trigger RAISE 中止（`commercial binding is unavailable`；send 另可能先被 0235 metering 拦截） | **预期行为**（验收 (d) 的注入点）：镜像 moderation 已有语义。**连锁回归面**：任何「enforcement ON + 无 binding + 走 insert_outboxed/edit/delete」的存量 db_test 会新失败——全量 `-- --ignored` 排查（多数 fixture 已带 binding，如 `message_quota_and_snaplink_outboxes_are_transactional`）。**新增 seam 面（裁决后）**：系统编辑驱动点 = recall_tests.rs:487/:500、recall_index_fence_tests.rs:181/:259；DM 创建驱动点 = dm/block_tests.rs:58/:79、dm.rs:437/:538/:639、group_dm.rs:826/:871/:960/:1022、call/security_tests.rs:43、barrier_tests.rs:109/:117、im-core db_tests.rs:779/:799、integration.rs:521——全部 fresh-DB 默认 enforcement OFF（无 RAISE，仅新增行写入）；逐一确认无 audit/outbox 行计数断言 |
| F5 | 重放/幂等命中（send 幂等键、delete 已删 `Ok(None)`、add_member 已存在 `Ok(false)`、edit 版本冲突 `Ok(None)`） | 不写 audit/outbox（无域变更） | 镜像 parity Replay half（A1-RP）；计数不变 |
| F6 | 双写防护（未来 trigger 扩 token / 重放撞 PK） | `ON CONFLICT (event_id) DO NOTHING` 幂等 | `duplicate_event_id_is_deduped_by_on_conflict` 语义；delete seam 的 `message.moderated` 跳过是语义硬线，双写仅在未来 trigger 扩展时可能且无害 |
| F7 | 进程在 tx commit 前崩溃 | 无行逃逸（原子性） | 与 moderation 路径同；relay 缺席时行保持 status 0 pending（T-11 形状），绝不假成功/假死 |
| F8 | `snaplink_commercial_runtime` 全局 singleton 被测试翻转未 restore | 后续消息 INSERT 全 raise（0235 metering） | 测序纪律：每测试开头防御性 re-assert OFF、结尾 `restore_enforcement_disabled`（:169-182 先例）；harness `--test-threads=1` 串行 |
| F9 | 拆模块搬移回归 | 既有 6 db_tests 零回归 | 搬移后**先单独跑** `audit_governance::` 再接线；机械搬移单独提交可回滚 |
| F10 | 词表并集漂移（sibling cfe64e80 的 `message.create` 等落地） | `governance_lane_for` 出现 10 臂，近义 token 各自 lane 相同——语义重复但无害 | 集成时以本验收为准手接 shared 文件（AGENTS §4.1） |

---

## 4. 迁移步骤

**DDL 迁移 = 零**（0239/0240/0241 已落地，不新增 `migrations/NNNN_*.sql`；`make migrate-smoke` 不受影响）。「迁移」= 代码/模块搬迁与接线顺序：

1. **拆模块（机械搬移，单独提交，可回滚）**：`audit_governance.rs`（1141 行）→ `audit_governance/{mod.rs, db_tests.rs}`——模块 doc 归 mod.rs，既有 fixtures 逐字入 db_tests.rs，`#[cfg(test)] mod db_tests;` 声明入 mod.rs。搬移后先 `cargo test -p aero-storage --lib audit_governance:: -- --ignored`（DATABASE_URL + 已迁移 throwaway 库）确认 6 测试零回归。勿与 in-flight 批次的其它结构动作叠加（AGENTS §4.4）。
2. **叶子 + 守卫**：model/audit.rs 5 常量 → 生产字面量点（crud.rs:378、authorization.rs:499）换常量 → truth-check-lib.sh 新 scan block（5 token + `path:line` allowlist）→ `scripts/truth-check.sh` 0 违规。
3. **lane 映射**：governance.rs 5 臂 + 2 单测换形 + per-token pin → `cargo test -p aero-ai governance::` 全绿。
4. **H3 repo**：`audit_governance/mod.rs` 落 `AuditGovernanceOutboxRepo::{new, append_in_tx, envelope_in_tx}`（复用 half-B 断言集为形状基准）→ `cargo check --workspace`。
5. **seam 接线（每接一条跑对应测试）**：delete（最简：捕获 AuditId + 条件 enqueue）→ send → edit → room.created → room.member.add → **S-1/S-2 系统编辑（F-1 裁决）→ S-3 DM 创建（F-2 裁决）**。
6. **验收 oracle**：`audit_governance/producer_tests.rs`（§5）→ 两条目 harness 过滤跑绿。
7. **全链门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo test --workspace --lib -- --ignored`（重点查 F4 连锁面）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 · `bash scripts/test-integration.sh`（`audit_governance::`、`moderation_finalize_outbox_parity`、`a3-relay-drill`、`t11-fail-closed`、`moderation-priority-drill` 全 PASS）。
8. **活验证**：全新一次性库 → `aero-cli migrate`（零新迁移，照例 build 先行）→ 跑两条目 → `DROP DATABASE`。

---

## 5. 可测验收映射（acceptance 原句 → 测试）

新测试统一命名 `moderation_finalize_outbox_parity_<op>`（`send`/`edit`/`delete`/`room_created`/`member_add`/`unfurl_edit`/`transcribe_edit`/`dm_created`/`group_dm_created`，共九 op——后四个为 F-1/F-2 裁决新增，§8.4），**前缀使两条 harness 条目同时命中**（`audit_governance::` 子串 + `moderation_finalize_outbox_parity` 子串）。每测试三分部（镜像 parity :247 的 Commit/Rollback/Replay 结构）；fixture = `fixture()` + 房间 + 消息（`MessageRepo::insert` 种子——无 seam 路径，隔离干净）；每测试开头 `reset_governance_table` + 防御性 re-assert enforcement OFF，结尾 `restore_enforcement_disabled`；断言全部 workspace/event_id 作用域。

| AC 分句 | 可测断言 | 位置 |
|---|---|---|
| **(a)** extend the `moderation_finalize_outbox_parity` in-tx oracle pattern to message.send/message.edited/message.deleted/room.member.add/room.created | 九 op 各驱动**生产 seam**：五 op 锚点 = E7/E8/E9（`insert_outboxed` / `edit_outboxed_authorized` / `soft_delete_outboxed_authorized` / `create_in_workspace_authorized` / `add_member_authorized`）；`unfurl_edit` 驱动 `edit_outboxed_system`、`transcribe_edit` 驱动 `update_voice_transcript_outboxed`（系统编辑 actor_id 断言 **NULL**）、`dm_created`/`group_dm_created` 驱动 `DmRepo::find_or_create_in_workspace` / `GroupDmRepo::find_or_create_in_workspace`（actor=发起者、detail.members 断言）。同事务断言恰 1 `audit_events` 行（action=常量、actor_id、target、workspace_id 全字段）+ 恰 1 outbox 行。**harness 双命中 + 空过滤守卫非空执行**（test-integration.sh:206） | `producer_tests.rs` |
| **(b)** status=0, class/priority matching governance_lane_for (backlog 10), payload.event_id == audit_events.id | 行断言：`status=0`（DDL 默认）；`class` == 叶子 `GOVERNANCE_CLASS_MESSAGE/ROOM`（governance.rs 同叶子常量 → 行值 == 叶子 == lane 值，链条闭环）；`priority=10`（comment-pin BACKLOG，half-B :543-547 惯例）；`payload.event_id` == `audit_events.id` 字符串 1:1；`payload.action` == 本地 token 原样；envelope 16 键逐字段（half-B :577-605 断言集）；T-11 形状：`attempts=0`、`last_error IS NULL`、claim 谓词（`status IN (0,1) AND available_at <= clock_timestamp() AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())`，pg.rs:104-117）选中该行。governance.rs per-token pin 钉 `governance_lane_for` 输出本身 | `producer_tests.rs` + governance.rs tests |
| **(c)** ROLLBACK removes both (no orphan outbox) | 回滚半部：置 `snaplink_commercial_runtime.enabled=TRUE` + fixture ws 无 binding → 同 op `Err`（0236 RAISE；send 另可能先被 0235 metering 拦截，同为 fail-closed）→ 域行 0（消息未插/未删/未编/房未建/员未加）+ `audit_events` 0 + outbox 0——两行同生共死 | `producer_tests.rs` |
| **(d)** Negative: an op with an unknown workspace aborts the tx (fail-closed), mirroring `moderation_finalize_without_binding_aborts_tx` | 镜像目标 = parity 的 Rollback half（:353，E10 勘误）；逐字镜像：删 binding → Err → 三零断言。另加 unknown-workspace 构造：`create_in_workspace_authorized` 传不存在 workspace id → `lock_workspace`（room/governance.rs:589）`WorkspaceNotFound` Err + 零行；delete 的 bogus-workspace 先例已由 `audit_failure_rolls_back_delete_and_outbox_append`（events.rs:607-647）钉住 | `producer_tests.rs` |
| **(e)** Keep harness b5_check slots green: relay-drill + t11-fail-closed unchanged, moderation-priority-drill `moderation-in-first-batch` PASS | 零改动清单（§2）保证三 drill 行为不变；新 seam 行 T-11 形状（(b)）；priority 10 ≤ 100 不扰动 DESC 优先。验证 = `bash scripts/test-integration.sh` 全链五条目 PASS | scripts/test-integration.sh（既有段，零改动） |

**Commit half 的运行环境**（额外非空证明）：enforcement OFF（fresh-DB 默认）**且无 binding** 下驱动五 op——同时证明无条件入队（R3 门语义：runtime disabled 也产 outbox 行）与 `source_system` 回退 `"aero-im"`。

---

## 6. 排序

1. 拆模块（§4.1，单独提交）
2. 叶子常量 + truth-check 守卫（§4.2）
3. governance.rs 5 臂 + 单测换形（§4.3）
4. H3 repo（§4.4）
5. seam 接线（§4.5，delete → send → edit → room.created → room.member.add → 系统编辑 → DM 创建）
6. producer_tests 验收 oracle（§4.6）
7. 全链门禁 + 活验证（§4.7-8）

**提交收敛**：拆分单独提交；接线 + 测试一批提交；`git diff` 应显示零迁移、零 connector 改动、零 harness 改动、零 ImService 改动。

---

## 7. 风险

- **F4 连锁回归面**（最高）：send/edit 首获 audit 行后，enforcement ON + 无 binding 的存量 db_test 开始 fail-closed——全量 `-- --ignored` 排查，逐一点验 fixture（多半已带 binding）。**裁决后新增驱动点**：系统编辑（recall_tests/recall_index_fence_tests）与 DM 创建（dm/block_tests、call/security_tests、barrier_tests、im-core db_tests、integration）——fresh-DB enforcement OFF 下仅多写行，无 RAISE；确认无行计数断言（§3 F4 清单）。
- **F-5 守卫（排除的持久化）**：legacy `RoomRepo::create_in_workspace` 的排除依赖「零生产调用者」事实——truth-check 新 scan block 钉其调用点 ∈ test-only `path:line` allowlist（照 §1.1 token 守卫同机制，drift 检查），防未来生产调用静默复活（§8.3 B 类行动项）。
- **拆模块回归**：机械搬移 1141 行；搬移后先跑 6 测试零回归再继续。
- **词表并集漂移**：sibling spec 落地时 `governance_lane_for` 可能出现两套近义 token 臂（语义重复无害）；集成时以本验收为准手接 shared 文件。
- **无条件入队 vs trigger Gate 1 不对称**：runtime disabled 时 moderation 行不进 outbox、message/room 行进——有意设计；若未来统一，改 0241 reconciler/trigger（出范围）。
- **`moderation_finalize_without_binding_aborts_tx` 仓外名**：只作验收原句保留，不新增该名测试；镜像对象 = parity rollback half + events.rs:607-647 先例。

---

## 8. 裁决记录 — audit_integrity_reviewer F-1/F-2（finalize 前并入）

> 裁决输入 = audit_integrity_reviewer（Q1 写路径枚举 + F-1…F-5）+ db_transaction_reviewer（锁序/原子性）+ perf_reviewer（热路径成本）。**最终范围 = 5 叶子 token 不变 + 8 条 seam 接线**（原 5 条 + S-1/S-2 系统编辑 + S-3 DM 创建，§1.4 表格）。

### 8.1 F-1（High）— unfurl/transcribe 系统编辑：裁决 = **接线（wire）**

**事实**（源码核验）：`edit_outboxed_system`（events.rs:42，unfurl_bot:189 调用）与 `update_voice_transcript_outboxed`（events.rs:~165，transcribe_bot:165 调用——**设计此前误标为 `edit_outboxed_system`，实为独立函数，本裁决修正命名**）是仓库中仅有的两条**消息体变更**路径（替换 blocks、更新 searchable_text、embedding=NULL、version+1、向全房广播 `RoomEvent::Edited`），零 audit 行——v1/v2 消费者都不可见。两函数都在自管 tx 内 `lock_message_in_tx`（FOR UPDATE），且已同 tx 写 event outbox + side effects——seam 位置天然 = commit 前。

**裁决**：两者都接入 `message.edited` seam（§1.4 S-1/S-2）。理由：

1. **内容一致性**：B5-1 的承诺是「消息体变更即审计」。用户 edit 与 bot edit 产生同构的用户可见变更（Edited 事件、searchable_text、版本号）；审计 feed 只见用户 edit 不见 bot edit，租户无法解释房内实际内容——正是 F-1 指出的 gap。
2. **系统参与者先例**：`message.moderated` 已由 worker/moderation_bot 经 `soft_delete_outboxed_system(…, audit_actor, audit_action)` 同事务写 audit 行——代码库已审计系统对治理消息态的变更，「system actor」不是既有边界。
3. **成本最低**：workspace 解析 = tx 内 `SELECT workspace_id FROM rooms WHERE id=$1`（消息行已 FOR UPDATE；rooms.workspace_id 写一次后不可变，无竞态）。非热路径（每消息一次性富集），perf_reviewer 的 round-trip 优化关切不适用。
4. **幂等天然**：`Ok(None)` 路径（已删/已 recall/版本冲突/unfurl 已带卡/无未转写 Voice 块）无域变更不写行（F5 语义）；unfurl loop-guard + transcribe `transcript.is_none()` 保证每消息至多一次变更 → 至多一行。
5. **actor 诚实**：`audit_events.actor_id` 可空（0007）+ `envelope_in_tx` 的 `actor: Option<ParticipantId>`——系统编辑 actor=None（envelope 侧 `AuditActor::system`），不冒认用户。
6. **token 集不变**：复用 `message.edited`（同域动作），detail.kind 区分（`unfurl` / `voice_transcript`）——零新叶子常量、零新 lane 臂、truth-check 面不变。
7. **失败语义自洽**：enforcement ON + 无 binding 下 bot 编辑 Err 回滚 → unfurl/transcribe 各自既有 Fail-open log-skip（消息不丢，仅卡不附）——与 §3 F4 的 send/edit 语义同构。

### 8.2 F-2（Medium）— DM/group-DM 创建：裁决 = **接线（wire）**；role/ownership 排除

**事实**：`DmRepo::find_or_create_in_workspace`（dm.rs:104，生产调用 aero-server dm.rs:95 `POST /api/dm`）与 `GroupDmRepo::find_or_create_in_workspace`（group_dm.rs:164，生产调用 group_dm.rs:192 `POST /api/group-dm`）是**用户驱动**的房间 + 成员边创建（default workspace），绕过 `create_in_workspace_authorized` / `add_member_authorized`——既不在 seam 也不在排除清单（F-2）。`create_in_workspace_authorized` 对 `RoomKind::Direct` 显式 `FixedMembership` 拒绝——DM 类房间别无审计入口。

**裁决**：两条 find_or_create 都接 `room.created` seam（§1.4 S-3），**每实际创建写一条**（与频道创建单行语义对齐：初始名册入 detail，不逐边写 member.add）。理由：

1. **用户驱动的一等产品动作**（打开 DM），不是系统生命周期——「system lifecycle」归类错误。
2. **可行性最简**：workspace 是函数入参（生产传 `DEFAULT_WORKSPACE_ID`），插入点显式，无需解析/锁序变更（advisory lock + FOR SHARE 序列已存在）。
3. **量级有界**：existing 早退（find 命中）无域变更 → 不写行（F5）；每成员对/组仅首开一次写行。
4. **排除即任意边界**：留下「频道创建审计、DM 创建不审计」的漂移形态——正是 F-5 警示的形状。

**明确排除**（入 §8.3 B 类，替代「等」）：

- `change_channel_member_role_authorized`（governance.rs:290）/ `transfer_channel_ownership_authorized`（:342）：既有边上的权限状态变更，可逆、非内容、边创建已审计——排除但**显式命名**（F-2 的「等」投诉即此）。
- `join_public_channel_authorized` / `leave_channel_authorized`：既有（已审计创建的）房间上的自助边增删——排除但显式命名；若未来治理要求，可复用 `room.member.add` token 低成本接线，不在本批次。

### 8.3 最终审计覆盖矩阵（audited / audited-excluded / system-lifecycle）

**A. 已审计（in-tx；seam 或既有机制）**

| 变更 | 生产路径（锚点） | 机制 | 说明 |
|---|---|---|---|
| message.send | `insert_outboxed`（idempotency.rs:158） | seam `message.send` | actor=sender；含 bot 经 ImService 发送的回复（agent_bot/ooo_bot）——如实归因 sender |
| message.edited（用户） | `edit_outboxed_authorized`（authorization.rs:399） | seam `message.edited` | 捕获 LockedRoomWriteAccess.workspace；Ok(None) 不写 |
| message.edited（系统：unfurl） | `edit_outboxed_system`（events.rs:42） | seam `message.edited` — **S-1（F-1 裁决）** | actor=None；detail `{"room_id","kind":"unfurl"}`；Ok(None)/版本冲突不写 |
| message.edited（系统：转写） | `update_voice_transcript_outboxed`（events.rs:~165） | seam `message.edited` — **S-2（F-1 裁决）** | actor=None；detail `{"room_id","kind":"voice_transcript"}`；changed=false 不写 |
| message.deleted（用户） | `soft_delete_outboxed_authorized` → `soft_delete_locked_outboxed_in_tx`（events.rs:293） | 既有 audit + seam outbox | 仅 `message.deleted` 补 outbox；`message.moderated` 分支跳过（trigger 唯一生产者，含 message_reports.rs:305） |
| message.recalled | `recall_locked_outboxed_in_tx`（authorization.rs:292） | 既有 in-tx audit（无 outbox） | v1 可见；v2 lane None（R-D2/T-11 负例，非缺口） |
| message.moderated | 0239 trigger（soft_delete_outboxed_system moderation 路 / ai worker / message_reports.rs:305） | trigger lane（seam 跳过） | ON CONFLICT 去重；0241 reconciler 覆盖 Gate 1 窗口 |
| room.created（工作区频道） | `create_in_workspace_authorized`（governance.rs:108） | seam `room.created` | actor=creator；`RoomKind::Direct` 被拒（FixedMembership） |
| room.created（1:1 DM 首开） | `DmRepo::find_or_create_in_workspace`（dm.rs:104） | seam `room.created` — **S-3（F-2 裁决）** | actor=发起者；detail 含 `members:[a,b]`；existing 早退不写 |
| room.created（group DM 首开） | `GroupDmRepo::find_or_create_in_workspace`（group_dm.rs:164） | seam `room.created` — **S-3（F-2 裁决）** | actor=creator；detail 含成员集；claim_exact 早退不写 |
| room.member.add | `add_member_authorized`（governance.rs:173） | seam `room.member.add` | 仅 inserted==true；direct/group-DM 拒绝 |

**B. 已审计排除（audited-excluded — 显式理由）**

| 变更 | 锚点 | 排除理由 |
|---|---|---|
| message.react / pin / unpin | reaction.rs:64 / pin.rs:31/:69 | 聚合/书签元数据：可逆、高频、非内容 |
| archive_channel | `set_channel_archived_authorized`（governance.rs:396） | 可逆 `is_archived` 状态旗标，非内容 |
| join_public_channel / leave_channel | governance.rs:213/:256 | 既有（已审计创建）房间上的自助边增删；可逆；actor=本人；房间创建已审计 |
| change_channel_member_role / transfer_channel_ownership | governance.rs:290/:342 | 既有边上的权限状态变更；可逆；非内容；边创建已审计（F-2 明确命名） |
| GroupDmRepo::patch_metadata（命名） | group_dm.rs:315 | 配置态（room.name），非成员/内容 |
| patch_channel / post_policy / slowmode / reaction_limit / retention | governance.rs:439+ | 工作区/频道配置态（topic 历史独立表） |
| legacy `RoomRepo::create_in_workspace` | room.rs:267 | **零生产调用者**（channels.rs:640 fixture、canvas.rs:470 test、db_tests）——无活缺口；**F-5 行动项**：truth-check allowlist 钉 test-only 调用点（照 AUDIT-FLAG 机制 + drift 检查），防未来生产调用静默复活 |
| crud.rs edit / update_voice_transcript | crud.rs:179/:435 | 零生产调用者（test/legacy） |

**C. 文档化系统生命周期例外（documented system-lifecycle exceptions）**

| 路径 | 锚点 | 理由 |
|---|---|---|
| retention sweep（过期软删） | workspace/sweep.rs:9 | 定时器按频道 retention 策略销毁；法务保全豁免已在 sweep 内判定；tombstone `Deleted` 事件照常扇出；无问责用户 actor；量级 = 整段历史（逐行审计 = 洪水） |
| ephemeral sweep（硬删） | message/sweep.rs:39 | 临时消息按设计不持久留痕；tombstone 扇出 |
| blob GC / GDPR force-delete | blob_gc_drain | 自有 blob ledger + 幂等 receipt 机制，非治理审计 |
| embedding backfill | crud.rs:474/:505 | 派生列（embedding），非内容 |
| cold-start 历史行（F-4） | — | seam 落地前的 audit 行（含存量 message.deleted）永无 v2 outbox 行：v1 车道已覆盖、0241 仅 moderation 范围——v2 从 t=0 断言 COUNT(outbox)==COUNT(audit) 必见历史洞；文档化 |
| 迟绑定 restamp（F-4） | — | 绑定前行永久 `"aero-im"`，绑定后行打真实 source_system——同 workspace 双 stamp 并存（行不重写；v1 reconciler 对同一 audit 行打真实 stamp = 跨车道不一致）；文档化 |

### 8.4 裁决对应的验收增量

- 新增 4 个 parity op（§5）：`unfurl_edit`（驱动 `edit_outboxed_system`）、`transcribe_edit`（驱动 `update_voice_transcript_outboxed`）、`dm_created`（驱动 `DmRepo::find_or_create_in_workspace`）、`group_dm_created`（驱动 `GroupDmRepo::find_or_create_in_workspace`）——前缀 `moderation_finalize_outbox_parity_` 使两条 harness 条目继续双命中。
  - Commit half（enforcement OFF + 无 binding）：各写恰 1 audit + 1 outbox；系统编辑 actor_id 断言 NULL；DM 断言 actor=发起者 + detail.members。
  - Rollback half（enforcement ON + 无 binding）：Err 回滚 → 域行 0 + audit 0 + outbox 0（DM：rooms/room_members 均 0）。
  - Replay half：系统编辑 = Ok(None) 路径（已删 / 无未转写块）；DM = existing 早退 → 0 + 0。
- §3 F4 连锁排查清单加入系统编辑（recall_tests.rs:487/:500、recall_index_fence_tests.rs:181/:259）与 DM 创建（dm/block_tests.rs:58/:79、dm.rs:437/:538/:639、group_dm.rs:826/:871/:960/:1022、call/security_tests.rs:43、barrier_tests.rs:109/:117、im-core db_tests.rs:779/:799、integration.rs:521）驱动点——fresh-DB 默认 enforcement OFF 无 RAISE，仅新增行写入；确认无 audit/outbox 行计数断言。
- F-5 行动项：truth-check 新 scan block 钉 legacy `create_in_workspace` 调用点 ∈ test-only allowlist（与 §1.1 token 守卫同机制）。
