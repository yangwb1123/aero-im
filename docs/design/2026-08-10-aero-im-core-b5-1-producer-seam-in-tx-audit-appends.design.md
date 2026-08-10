# Design — aero-im-core B5-1：message.*/room.* 写路径同事务 audit append（producer seam，S1–S4 + R1 拆模块 + R8/R9 oracle）

- **Direction**: "Land the B5-1 producer seam: in-tx audit appends + outbox enqueue on ImService message.*/room.* write paths"
- **Module (analysis root)**: `crates/aero-audit-connector`（生产者契约面；交付面 = `crates/aero-storage` 四条 seam 接线 + 模块拆分 + producer db_tests + drill 服务路径腿）
- **Requirements**: `docs/auto/runs/land-the-b5-1-producer-seam-in-tx-audit-appends--aa8d398b/artifacts/requirements-10762e10/requirements.md`
- **Superseded design anchor**: `docs/design/2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.design.md`（其 token 集 `message.send`/`room.created`/`room.member.add` 已被落地 SQL allowlist 取代；F-1/F-2 裁决（系统编辑/DM 创建接线）**不在本 direction 验收内**——本设计以其落库形态为准）
- **Campaign**: `aero-im-b5-outbox-relay`
- **Status**: Design（证据核验完成 2026-08-10；三处证据勘误并入 §0：R1 拆模块尺寸不可行、R2 双调用点、R5 错误映射）

---

## 0. 证据核验（evidence = untrusted claims，逐条对源码）

全部核心锚点**命中**；路径漂移（E3/E4/E5/E8）与仓库状态更正（0242/0245/0246 已落地、delete/recall 已审计、file-size 现红）经源码复核**属实**。另发现证据自身的**三处新勘误**（§0.1 C1–C3）。

| # | 证据声明 | 结论 | 核验要点 |
|---|---|---|---|
| E1 | 迁移计数 244；0242/0245/0246 落地 | ✅ | `ls migrations/*.sql | wc -l` = 244；0242/0245/0246 存在且头注释声明 **trigger-only ownership**（"Rust NEVER writes the outbox for them"）；harness arbiter test-integration.sh:606-607 钉 244 |
| E2 | `insert_outboxed` idempotency.rs:158-246 零审计 | ✅ | 结构逐行相符：幂等早退（tx 之前）→ `lock_effective_sender_room_access`（**bool 折叠、workspace 丢弃**）→ reply/attachment 锁 → `insert_row_in_tx` → `EventOutboxRepo::insert_in_tx` → `MessageSideEffectRepo::insert_in_tx` → `message_send_keys` claim → commit。`rg AuditRepo idempotency.rs` = 0 |
| E3 | `edit_outboxed_authorized` authorization.rs:399-453 零审计 | ✅ | `lock_effective_message_write_access(...).await?.is_none()`（:425-428）丢弃 workspace；`Ok(None)`（:438-452）commit 前早退；`if edited.is_some() { tx.commit() }`（:449-452） |
| E4 | delete/recall 已审计、出范围 | ✅ | `soft_delete_outboxed_authorized` :454-507 传 `Some("message.deleted")` + detail 进 in-tx append；recall :364-372 已写 `LOCAL_ACTION_MESSAGE_RECALLED`。`message.deleted` 保持 unmapped（R-D2，0245 头注释钉） |
| E5 | `append_in_tx` :127 签名 | ✅ | `(tx, workspace, actor: Option<ParticipantId>, action: &str, target: Option<&str>, detail: serde_json::Value) -> Result<AuditId, sqlx::Error>`；`append_on` 内 `created_at` 由 **app 进程 now_utc() 盖章**（R8 merge 测试同窗前提的关键事实） |
| E6 | 叶子 token 已存在 | ✅ | `aero-common/src/model/audit.rs:156-227`：`LOCAL_ACTION_MESSAGE_CREATE/EDIT`（:168/:170）、`LOCAL_ACTION_ROOM_CREATE/ARCHIVED`（:198/:200）、`LOCAL_ACTION_MESSAGE_RECALLED`（:215）、`AGGREGATED_MESSAGE_ACTION`、`L1_WINDOW_SECONDS=60`、`GOVERNANCE_CLASS_*`、`AUDIT_SOURCE_SYSTEM`。**零叶子改动** |
| E7 | lane 映射现状 | ✅ | `aero-ai/src/governance.rs:82-111`：moderated→admin/100、room.create→room/10、room.archived→room/10；message.create/edit 刻意 unmapped（SQL-side allowlist，:116 注释）。**零 lane 改动** |
| E8 | `audit_governance.rs` 2626 行 HARD 违规 | ✅ | `wc -l` = 2626；`scripts/file-size-check.sh` 现红（HARD 1200，**无测试文件豁免**）；模块 doc 含 H3 手记原文；db_tests 结构：fixtures :42-251（`pool`/`fixture`/`message_in_workspace`/`enable_enforcement_with_binding`/`restore_enforcement_disabled`/`moderate_finalize`/`count_governance_rows`/`governance_rows_for`/`reset_governance_table`）+ 测试 :252-2626（parity :252 / ddl :733 / gates :891-1067 / dedup :1068-1339 / l1 :1340-1866 / lanes :1867-2626，含 `room_lane_outbox_parity` :1921 / `recall_lane_outbox_parity` :2142 / `message_lane_outbox_parity` :2367 / `room_lane_unconditional_enqueue` :2563） |
| E9 | rollback 注入通道 = 0236 binding RAISE | ✅ | `audit_events_snaplink_delivery`（0236）AFTER INSERT：`snaplink_commercial_runtime.enabled=TRUE` 时调用 `aero_snaplink_binding_for_workspace`（0235:193，无 binding → `RAISE P0001 'commercial binding is unavailable'`）→ **中止 audit INSERT 及整个外层 tx**。0242/0245 自身无 runtime gate、无 binding 查询（头注释 D2 先例） |
| E10 | harness 槽位 / pg.rs 优先 | ✅ | test-integration.sh:318-325 两条 `run_migrated_integration`（`audit_governance::` / `moderation_finalize_outbox_parity`）；:620-630 drill 调用（`cargo run --quiet -p aero-audit-connector --bin aero-audit-l1-parity-drill`）；b5-pin.sh 名单含 `audit_governance::`/`moderation_finalize_outbox_parity`/`l1-aggregation-drill`/`room_lane_outbox_parity`/`message_lane_outbox_parity`；pg.rs:546 `mixed_priority_claim_orders_moderation_first_then_fifo` 存在 |
| E11 | 先例：delete in-tx append + rollback 镜像 | ✅ | `soft_delete_locked_outboxed_in_tx` events.rs:299-355（:337-344 裸 `append_in_tx(...).await?;` 丢弃 AuditId）；`audit_failure_rolls_back_delete_and_outbox_append` events.rs:614-647（bogus workspace → FK Err → 消息保留 + event_outbox 计数不变） |
| E12 | drill 现状 | ✅ | `aero-audit-l1-parity-drill.rs` 325 行：自隔离 start（TRUNCATE outbox + 删自家 audit 行）→ 直接 SQL INSERT 自种 N=5 行 `message.create`（固定 created_at）→ `parity()`（workspace 作用域 + 窗口起点 cutoff 双侧）→ spill 腿（status=2 强制 + 同窗迟到行 → 恰 1 spill 行，确定性 key 复算）。**`parity()` 对 leg 2 直接复用**（ws 参数化） |
| E13 | 依赖方向 | ✅ | `aero-storage/Cargo.toml` 仅 aero-common + infra（无 aero-auth / aero-audit-connector）→ aero-audit-connector 加 aero-storage dev-dep 无环 |
| E14 | truth-check 3f | ✅ | `scripts/truth-check-lib.sh:162/:357` Room allowlist-token guard：`"room.create"`/`"room.archived"` 生产字面量在叶子外 = 违规——seam 必须用叶子常量 |

### §0.1 证据勘误（本设计采纳的修正）

| # | 勘误 | 证据原文 | 修正 |
|---|---|---|---|
| **C1** | **R1「db_tests.rs 逐字搬移」尺寸不可行** | R1: "`db_tests.rs` — existing `db_tests` module **verbatim**" | 1:1 搬移 = ~2585 行 > 1200 HARD（file-size-check.sh **无测试文件豁免**，`find crates -name '*.rs'` 全扫）→ **必须拆成 `db_tests/` 子模块目录**（§4.1 布局）。测试体逐字保留，仅模块外壳变更；harness 过滤是**全路径子串匹配**（`audit_governance::` 命中 `audit_governance::db_tests::parity::…`），契约不破。仓内先例：`room/governance.rs:797 mod db_tests;` + `room/governance/db_tests.rs`（1068 行）、aero-ai `db_tests.rs`（403）+ `db_tests/moderation_finalize_drill_tests.rs`（651） |
| **C2** | **R2 辅助函数双调用点** | R2: "Update the single call site (:182)" | `lock_effective_sender_room_access` 有**两**个调用点：`insert_idempotent`（:176）与 `insert_outboxed`（:182），签名变更必须同时改两处；`insert_idempotent` 保持 bool 语义（`let Some(_) = … else { rollback; return Forbidden }`） |
| **C3** | **R5 错误映射缺失** | R5 未提 map | `set_channel_archived_authorized` 返回 `Result<(), RoomMembershipWriteError>`（非 sqlx::Error），append 必须 `.map_err(map_storage_error)?`（与 R4 同） |
| C4 | （细化）send 路径 rollback 的 RAISE 源 | §0.7 "0236 binding RAISE on the audit INSERT" | send 路径可能**先**被 0235 `messages_snaplink_metering`（messages INSERT 时触发，enforcement ON + 无 binding → `RAISE P0001`）拦截——在 audit append 之前。两种源结果一致（Err → 整 tx 回滚 → 零行），断言**以 outcome 为准**（§3 F4）；room.create 路径无 messages INSERT，专练 0236 审计通道 |

---

## 1. API 变更

**零公开 API 变更**（`MessageRepo`/`RoomRepo` 签名、`ImService`、REST/WS 全冻结）。变更全在 `crates/aero-storage` 私有实现 + 1 个 bin 的 dev-dependency。

### 1.1 `lock_effective_sender_room_access` 签名（内部 helper，R2 前置）

`crates/aero-storage/src/message/idempotency.rs:14-23`：

```rust
async fn lock_effective_sender_room_access(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    room: aero_common::RoomId,
    sender: ParticipantId,
) -> Result<Option<crate::message::authorization::LockedRoomWriteAccess>, sqlx::Error> {
    crate::message::authorization::lock_effective_message_write_access(
        tx, room, sender, crate::message::authorization::PostPolicy::Enforce,
    )
    .await
}
```

（`LockedRoomWriteAccess { pub workspace: WorkspaceId }` 为 `pub(crate)`，authorization.rs:20——同 crate 可用，零可见性变更。）**两个调用点同步改写**（C2）：

```rust
// insert_idempotent（保持原语义）：
let Some(_) = lock_effective_sender_room_access(&mut tx, room, sender).await? else {
    tx.rollback().await?;
    return Err(Error::Forbidden(
        "room access or posting authority was revoked before the message could commit".into(),
    ));
};
// insert_outboxed（同形，绑定 access 供 seam 使用）：
let Some(access) = lock_effective_sender_room_access(&mut tx, room, sender).await? else { …同上… };
```

失败语义与今日逐字一致（Forbidden + rollback）。

### 1.2 S1 — `message.create` append（`insert_outboxed`）

`crates/aero-storage/src/message/idempotency.rs`，在 `MessageSideEffectRepo::insert_in_tx(...)`（:212-222）之后、`claimed` 判定（:227）之前（commit :246 之前）：

```rust
// idempotency.rs 顶部 import 增补：use aero_common::{…, LOCAL_ACTION_MESSAGE_CREATE};
let _audit_id = crate::audit::AuditRepo::append_in_tx(
    &mut tx,
    access.workspace,
    Some(sender),
    LOCAL_ACTION_MESSAGE_CREATE,          // 叶子常量——裸字面量违规（truth-check 3f）
    Some(&message.id.to_string()),
    serde_json::json!({ "room_id": message.room_id }),
).await?;                                   // Err → 整 tx 回滚（fail-closed）
```

- AuditId 丢弃（无 Rust 消费者；消费者 = 0242 trigger）。`?` 经 `From<sqlx::Error>` 直接上抛（与同函数既有 `sqlx::query(...).await?` 一致）。
- 幂等重放早退（:166-176，tx 之前）与 claim-loser 回滚（:242-244）**不写**任何 audit 行（无域变更）。

### 1.3 S2 — `message.edit` append（`edit_outboxed_authorized`）

`crates/aero-storage/src/message/authorization.rs:399-453`：

1. :425-428 改为捕获（`authorization.rs` 已 import `LOCAL_ACTION_MESSAGE_RECALLED`，增补 `LOCAL_ACTION_MESSAGE_EDIT`）：

```rust
let Some(access) =
    lock_effective_message_write_access(&mut tx, resolved_room, actor, PostPolicy::Enforce).await?
else {
    return Err(Error::Forbidden(
        "message edit authority was revoked before commit".into(),
    ));
};
```

2. :449-452 改为：

```rust
if edited.is_some() {
    let _audit_id = crate::audit::AuditRepo::append_in_tx(
        &mut tx,
        access.workspace,
        Some(actor),
        LOCAL_ACTION_MESSAGE_EDIT,
        Some(&id.to_string()),
        serde_json::json!({ "room_id": resolved_room }),
    ).await?;
    tx.commit().await?;
}
```

- `Ok(None)`（version 冲突 / 已删 / 已 recall）**不写**（无域变更）。`existing.sender_id != actor` 等 Err 分支天然在 append 之前。

### 1.4 S3 — `room.create` append（`create_in_workspace_authorized`）

`crates/aero-storage/src/room/governance.rs:108-153`，owner-member INSERT（:141-150）之后、commit（:151）之前（governance.rs 顶部 import 增补 `LOCAL_ACTION_ROOM_CREATE`）：

```rust
crate::audit::AuditRepo::append_in_tx(
    &mut tx,
    workspace,                                // 函数入参，无需解析
    Some(creator),
    LOCAL_ACTION_ROOM_CREATE,
    Some(&id.to_string()),
    serde_json::json!({
        "room_id": id,
        "kind": room_kind_str(kind),          // RoomKind 是 Copy，kind 仍可用
        "name": name,                         // Option<String> → None 时 JSON null
    }),
)
.await
.map_err(map_storage_error)?;                 // RoomMembershipWriteError 域
```

- `RoomKind::Direct` 仍被 :110-112 `FixedMembership` 拒绝——DM 创建保持出范围（F-2 裁决不在本批）。

### 1.5 S4 — `room.archived` append（`set_channel_archived_authorized`）

`crates/aero-storage/src/room/governance.rs:396-417`，`update_one(...)`（:409-415）之后、commit（:416）之前：

```rust
crate::audit::AuditRepo::append_in_tx(
    &mut tx,
    workspace,                                // lock_channel_aggregate 结果（:401）
    Some(caller),
    LOCAL_ACTION_ROOM_ARCHIVED,
    Some(&room.to_string()),
    serde_json::json!({ "room_id": room, "archived": archived }),
)
.await
.map_err(map_storage_error)?;
```

- `update_one` 已保证 `rows_affected()==1`（否则 Err）→ **每次成功 UPDATE 恰 1 条 audit 行**：archive（`archived=true`）与 unarchive（`archived=false`）两方向同 token，detail 携带新旗标。

### 1.6 模块拆分布局（R1，C1 修正版）

```
crates/aero-storage/src/audit_governance/
├── mod.rs           模块 doc（现文件头部 :1-40 原文，含 H3 手记 verbatim）
│                    + #[cfg(test)] mod db_tests;
│                    + #[cfg(test)] mod producer_tests;
├── db_tests.rs      共享 fixtures + use 块（:29-251 原文）+ 子模块声明（~250 行）
└── db_tests/        既有测试体逐字迁入（每个 < 1200 HARD，目标 < 800 免 WARN）
    ├── parity.rs    moderation_finalize_outbox_parity + rust_produced_payload_matches_0239_envelope（~480 行）
    ├── ddl.rs       ddl_contract_defaults_and_checks（~160 行）
    ├── gates.rs     runtime_disabled + non_moderation + auth_tokens（~180 行）
    ├── dedup.rs     duplicate + reconcile（~270 行）
    ├── l1.rs        l1_aggregate_migrated + insert_audit_row + recompute_window_key + 3 个 l1 测试（~530 行）
    └── lanes.rs     room_trigger_migrated + insert_audit_row_returning_id + room/recall/message_lane 4 测试（~760 行）
└── producer_tests.rs  新增 R8 八测试（~350 行）
```

- 每个子模块文件首行 `use super::*;`（fixtures/use 在 db_tests.rs 根部，逐字保留）；测试体**字节级不变**，仅模块外壳（wrapper + `use super::*;`）变更。
- 测试全路径变为 `audit_governance::db_tests::parity::moderation_finalize_outbox_parity`——harness 子串过滤 `audit_governance::`（test-integration.sh:320）与 `moderation_finalize_outbox_parity`（:324）**双命中不变**。
- `crates/aero-storage/src/lib.rs:11` `pub mod audit_governance;` 不变（文件→目录模块，路径不变）。
- 拆分子模块边界按 **helper 使用域**划分（`insert_audit_row` 在 l1.rs 域内、`insert_audit_row_returning_id`/`*_trigger_migrated` 在 lanes.rs 域内），子模块间零互引。

### 1.7 drill 服务路径腿（R9）

- `crates/aero-audit-connector/Cargo.toml` `[dev-dependencies]` 增 `aero-storage.workspace = true`（无环：aero-storage 仅依赖 aero-common + infra，E13）。
- `crates/aero-audit-connector/src/bin/aero-audit-l1-parity-drill.rs`：
  - 头注释（:11-35）增补：seed-vs-service gap 已闭合，leg 2 经真实 send 路径产行。
  - **Leg 2**（spill 腿之后，复用现有 `parity()` 与 `retention_cutoff_epoch()`）：
    1. 自隔离：新 ws2/actor2（UUID v4）；fixture 行 = participant + workspace + `workspace_members(ws2, actor2, 'owner')` + `rooms(id, kind='group', name, created_by=actor2, workspace_id=ws2)` + `room_members(room, actor2, 'owner')`——**直接 SQL**（沿用 leg 1 风格；不得走 `create_in_workspace_authorized`，否则自产 room.create audit 行污染计数）。
    2. `let cutoff = retention_cutoff_epoch(&pool, retention_days).await?;`
    3. **N 次真实发送**（N = `AERO_AUDIT_DRILL_ROWS`，默认 5）：

```rust
use aero_storage::{MessageRepo, NewMessage};
use aero_common::Block;

let repo = MessageRepo::new(pool.clone());
for _ in 0..rows {
    repo.insert_outboxed(
        NewMessage {
            room_id,
            sender_id: actor2,
            blocks: vec![Block::text("l1-parity-drill")],
            reply_to: None,
            metadata: serde_json::Value::Null,
            expires_at: None,
        },
        None,      // 无幂等键：每次都是新消息
        Vec::new(),
        None,
    ).await.context("service-path send")?;
}
```

    4. `let (sum, count) = parity(&pool, &ws2, cutoff).await?;` 断言 `sum == count` **且** `sum == rows`。窗口边界跨越安全：每个 audit 行恰落一个 window/spill 行，SUM 侧天然守恒；`sum != rows` 守卫使腿**非空**（seam 缺席时 COUNT=0 ≠ 5 → 红）。
    5. 不重跑 spill 腿（其同窗前提需固定 created_at，仅 leg 1 可保证）。
  - 维持现有 SKIP（表/函数缺失 exit 2）与 leg 1 逐字节不变。
  - 文件最终 ~410 行，< 800 无尺寸警告。

---

## 2. 兼容性约束

| 面 | 约束 | 机制 |
|---|---|---|
| **DDL/迁移** | 零迁移，计数保持 244 | 无 `migrations/NNNN_*.sql`；0242/0245 已落地且 trigger-only（R6：**Rust 永不写 `audit_governance_outbox`**——本批只写 `audit_events`，outbox 行由 AFTER INSERT trigger 同 tx 物化；`ON CONFLICT (event_id) DO NOTHING` 仅是未来双写兜底，不是回退通道） |
| **ImService/API/WS** | 调用形状冻结 | seam 全在 storage tx 内；`send_message_inner`（im-core messages.rs:139-216）零改动 |
| **connector/relay** | 除 drill + dev-dep 外零改动 | client/pg/outbox/config/fake/stub 不动；`claim_due` ORDER BY priority DESC 不动；新行 priority 10 < 100 不扰动 moderation 优先（pg.rs:546 逐字保持） |
| **harness** | 零槽位改动 | 新测试模块 `audit_governance::producer_tests` + 前缀 `moderation_finalize_outbox_parity_` → 两条目（:320/:324）非空命中；空过滤守卫（:206）不红 |
| **叶子/lane** | 零改动 | token 全存在；`governance_lane_for`/`is_admin_class` 不动；class 用叶子 `GOVERNANCE_CLASS_*`；priority 10 从行值断言，**不 import**（aero-storage 不得依赖 aero-ai） |
| **钉位测试** | 逐字保持 | `moderation_finalize_outbox_parity`、`ddl_contract_defaults_and_checks`、`room_lane_outbox_parity`、`recall_lane_outbox_parity`、`message_lane_outbox_parity`、`room_lane_unconditional_enqueue`、`audit_failure_rolls_back_delete_and_outbox_append` 等全部字节级保留（R1 只动外壳） |
| **deps** | aero-audit-connector dev-dep 单向 | aero-storage → aero-common + infra（E13），无环；lockfile 无新条目（workspace 成员） |
| **v1 共存** | 新 audit 行照常过 0236 v1 通道 | 0236 `audit_events_snaplink_delivery` 对每行触发（enforcement OFF → RETURN NEW）；不重定向、不重复 |

---

## 3. 失败模式

| # | 场景 | 行为 | 关系 |
|---|---|---|---|
| F1 | seam 内 audit INSERT 失败（约束/RAISE） | `Err` → 整 tx 回滚：域行 0 + event_outbox 0 + audit 0 + trigger outbox 0 **同生共死** | 镜像 `audit_failure_rolls_back_delete_and_outbox_append`（events.rs:614-647）；"a message is never silently deleted unaudited"（crud.rs:359-363）扩展到写入侧 |
| F2 | enforcement ON + 无 binding 的 workspace 上 send | `Err`（C4：**0235 metering 先于 audit append 拦截**，或 0236 在 audit INSERT 处 RAISE——两种源，同一 outcome）→ 零行逃逸 | 预期 fail-closed；R8 `_send_rollback` 的注入点 |
| F3 | enforcement ON + 无 binding 的 workspace 上 room.create | `Err`（0236 在 audit INSERT 处 RAISE——本路径无 messages INSERT，**纯审计通道**）→ rooms 0 + room_members 0 + audit 0 + outbox 0 | R8 `_room_create_rollback` 的注入点 |
| F4 | **连锁回归面**：既有 db_test 在 enforcement ON + 无 binding 下驱动 send/edit/room 写 | 新失败（此前这些路径不写 audit 行，不触发 0236） | 缓解：全量 `cargo test --workspace --lib -- --ignored` 排查；fresh-DB 默认 enforcement OFF（`snaplink_commercial_runtime` singleton 默认 `enabled = FALSE`，0235:10） |
| F5 | 无域变更路径 | **零 audit 行**：幂等重放早退（send）、claim-loser 回滚、`Ok(None)` edit（version 冲突/已删/已 recall）、`update_one` 0 行 | F5 语义保持；R8 `_send_replay` 钉 |
| F6 | merge 测试窗口跨越（60s 边界） | 同窗两写间隔为毫秒级（`append_on` 用 app `now_utc()` 盖章），跨越概率可忽略；若发生 = 断言红（可重试，非假绿） | R8 `_send_merge` 以"恰 1 window 行 count=2"为断言；SUM 守恒始终成立 |
| F7 | 进程在 commit 前崩溃 | 无行逃逸（tx 原子性）；未认领行保持 status 0 pending（T-11 形状） | 与 moderation 路径同 |
| F8 | 全局 singleton 翻转未 restore | 后续消息 INSERT 全 raise（0235） | 测序纪律：每测试开头防御性 re-assert OFF + 结尾 `restore_enforcement_disabled`（db_tests :125-187 先例）；harness `--test-threads=1` 串行 |
| F9 | 拆模块搬移回归 | 既有 15 测试零回归 | 搬移后**先单独跑** `audit_governance::` 过滤（DATABASE_URL + 已迁移 throwaway 库）再接线；拆分单独提交可回滚 |

---

## 4. 迁移步骤

**DDL 迁移 = 零**（244 不变）。「迁移」= 代码布局 + 接线顺序：

1. **R1 拆模块**（单独提交，可回滚）：按 §1.6 布局；`git mv` 语义 + 外壳改写。**门禁**：`cargo check --workspace` 干净 + `cargo test -p aero-storage --lib audit_governance:: -- --ignored --test-threads=1`（DATABASE_URL + 已迁移 throwaway 库）全部绿（含 parity/room/message/recall lane 四钉位），**零回归后才接线**（AGENTS §4.4：不与其它结构动作叠加——注意仓内另有 in-flight 的 aero-ai db_tests 拆批，互不相交，勿混入本提交）。
2. **R2 接线**（send）：helper 签名 + 双调用点 + S1 append → 单测/既有 db_tests 绿。
3. **R3 接线**（edit）：capture + S2 append → 绿。
4. **R4/R5 接线**（room.create / room.archived）：S3/S4 append（含 C3 的 `map_err`）→ 绿。
5. **R8 producer_tests**（`audit_governance/producer_tests.rs`，§5 八测试）→ `cargo test -p aero-storage --lib moderation_finalize_outbox_parity -- --ignored --test-threads=1` 绿（两条目过滤均非空）。
6. **R9 drill leg 2**：dev-dep + 头注释 + leg 2 → 本地 `DATABASE_URL=… cargo run --quiet -p aero-audit-connector --bin aero-audit-l1-parity-drill` PASS。
7. **全链门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo test --workspace --lib -- --ignored`（F4 排查）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（file-size：audit_governance.rs 红消除；web/app.js 存量 JS 红不动，属 pre-existing）· `bash scripts/test-integration.sh`（`audit_governance::`、`moderation_finalize_outbox_parity`、`l1_window_aggregates_`、`l1-aggregation-drill`、room/message-lane、`moderation-priority-drill`、`t11-fail-closed` 全 PASS）。
8. **活验证**：全新一次性库 → `cargo build`（迁移编译期嵌入，AGENTS §4.2）→ `aero-cli migrate`（零新迁移）→ 跑 §7 关键段 → `DROP DATABASE`。

---

## 5. 可测验收映射（supplied acceptance → executable checks）

新测试全部位于 `audit_governance/producer_tests.rs`，命名前缀 `moderation_finalize_outbox_parity_`（harness 双条目命中）；统一三分部（Commit/Rollback/Replay）+ 每测试开头 `reset_governance_table` + 防御性 re-assert `snaplink_commercial_runtime.enabled = FALSE`，结尾 restore；fixture = 自有 `pool()` + 扩展 `fixture()`（workspace + participant + workspace_members + room（**经裸路径 `RoomRepo::create_in_workspace` 或直接 SQL 创建——不得走已接线的 authorized 路径，防自污染**）+ room_members owner 边）；断言全部 workspace/event_id 作用域。

| Supplied acceptance | Executable form（测试名 + 核心断言） |
|---|---|
| **db_tests（throwaway DB, `#[ignore]`+DATABASE_URL）**：one send tx commits exactly 1 `message.create` audit row AND 1 L1 window outbox row per (workspace, 60s window) with count increments on merge（钉 0242 allowlist via `LOCAL_ACTION_MESSAGE_CREATE`） | `moderation_finalize_outbox_parity_send`：1 tx ⇒ 恰 1 audit 行（workspace、`action = LOCAL_ACTION_MESSAGE_CREATE`、actor=sender、target=message id、detail.room_id）**且**恰 1 message-class window 行（`aggregated='true'`、`count=1`、class=`GOVERNANCE_CLASS_MESSAGE`、priority 10、status 0、`attempts=0`、`last_error IS NULL`、T-11 claim 谓词选中）+ 1 event_outbox 行。`_send_merge`：同 60s 窗两次 `insert_outboxed`（毫秒级间隔，`append_on` app 盖章）⇒ 恰 1 window 行 `count=2`（merge 递增），2 audit 行 |
| **room.create/room.archived each yield a 1:1 class 'room' outbox row（钉 0245）** | `_room_create`：`create_in_workspace_authorized` ⇒ 1 audit 行（`action=LOCAL_ACTION_ROOM_CREATE`、actor=creator、detail.kind/name）+ **1:1** outbox 行（class `room`、priority 10、status 0、`payload.event_id == audit_events.id`、无 aggregated/spill 键）。`_room_archive`：`set_channel_archived_authorized(room, caller, true)`（fixture room kind='channel'、caller 持 room owner 边）⇒ 1 audit 行（`action=LOCAL_ACTION_ROOM_ARCHIVED`、detail.archived=true）+ 1:1 行；再调 `false` ⇒ 第二条同 token 行、`detail.archived=false` |
| **audit-FK failure rolls back message + outbox + event_outbox together** | `_send_rollback`：`enabled=TRUE` + fixture ws 无 binding ⇒ `insert_outboxed` 返 `Err`（C4：0235/0236 任一 RAISE 源）⇒ messages 0、event_outbox 0、audit_events 0、governance outbox 0（workspace 作用域）。`_room_create_rollback`：同注入 ⇒ `Err` ⇒ rooms 0、room_members 0、audit 0、outbox 0（纯 0236 通道）。FK 字面通道仍由未改的 `audit_failure_rolls_back_delete_and_outbox_append`（events.rs:614-647）钉住 |
| **Drill acceptance**: `aero-audit-l1-parity-drill` re-run where rows are produced through the real send path instead of direct SQL seed — parity `COUNT(outbox) == COUNT(audit)` for the mapped subset | R9 leg 2：`insert_outboxed` ×N（N=5 默认）⇒ `parity(&pool, &ws2, cutoff)` 断言 `SUM == COUNT == N`；`sum != rows` 守卫非空（seam 缺席 → COUNT=0 ≠ 5 → 红）；头注释 gap 闭合说明 |
| **T-11/37-tests mapping**: 新测试 slot 进既有 harness 槽位；moderation-priority 语义不动 | 命名前缀 + 模块 `audit_governance::producer_tests` ⇒ test-integration.sh:320/:324 双条目非空匹配，零槽位改动；新行 priority 10 在 DESC 下永不抢占 admin 100（`mixed_priority_claim_orders_moderation_first_then_fifo` pg.rs:546 不动且绿）；"37/37" 仓外契约文本不重议（E6） |
| **R8 附加**：`_edit` | `edit_outboxed_authorized`（先 send 种子）：1 audit 行 `action=LOCAL_ACTION_MESSAGE_EDIT`、actor=editor；merge 进 L1 窗（count 递增）；`Ok(None)`（错 `expected_version`）⇒ 0 audit 行 |
| **R8 附加**：`_send_replay` | 同一 `client_message_id` 两次 `insert_outboxed` ⇒ 1 message、**1** audit 行、window count 1（幂等早退零写入——F5） |

**Commit half 运行环境**（额外非空证明）：enforcement OFF（fresh-DB 默认）**且无 binding** 下驱动四条 seam——同时证明无条件入队（0242/0245 无 runtime gate）与行形状（priority 10、source_system 由 trigger 常量写死）。

---

## 6. 排序 & 提交收敛

1. R1 拆模块（§4.1，**单独提交**；`audit_governance::` 过滤全绿后才继续）
2. R2 → R3 → R4 → R5 接线（每步编译 + 对应测试绿；delete 已审计故不需先例步骤）
3. R8 producer_tests + R9 drill leg（+ dev-dep + 头注释）
4. 全链门禁 + 活验证（§4.7-8）

**提交收敛**：`git diff` 应显示零迁移、零 harness 改动、零 ImService/connector（除 drill）改动、零叶子/lane 改动。改动文件清单：`aero-storage/src/audit_governance/`（新目录）、`aero-storage/src/message/idempotency.rs`、`aero-storage/src/message/authorization.rs`、`aero-storage/src/room/governance.rs`、`aero-audit-connector/Cargo.toml`（dev-dep）、`aero-audit-connector/src/bin/aero-audit-l1-parity-drill.rs`、本设计文档（+ 后续需求/验收文档）。

---

## 7. 风险

- **F4 连锁回归面（最高）**：send/edit/room 写首获 audit 行后，enforcement ON + 无 binding 下驱动的存量 db_test 新失败。缓解：全量 `-- --ignored` 排查（多数 fixture 已带 binding；fresh-DB 默认 OFF）。
- **R1 尺寸红线（C1）**：若按证据原样 1:1 搬移 db_tests → 2585 行 HARD 自违规；本设计的分目录拆解使每个文件 < 800（免 WARN）。拆后立即跑 15 测试零回归再接线。
- **Drill leg 2 空转**：`sum != rows` 守卫杜绝 `0 == 0` 假绿；leg 2 不重跑 spill 腿（同窗前提仅 leg 1 固定 created_at 可保证）。
- **窗口跨越**：`_send_merge` 精确断言 1 window 行 count=2——毫秒级间隔对 60s 窗，实际不可达；万一红可重试（SUM 守恒断言始终成立）。
- **并行批次协调**：仓内 in-flight aero-ai db_tests 拆批（untracked）与 aero-storage 拆分**不相交**；集成时 `git reset --hard master` 校准基线 + 手接共享文件（AGENTS §4.1）；最终全量门禁会同时验两批。
- **仓外 "37/37" 文本**：不变更、不重议（E6）；本批只钉仓内子集（命名槽位 + drill）。
