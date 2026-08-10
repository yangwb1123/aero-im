# Requirements Spec — message.*/room.*/admin.* 车道映射 + in-tx `AuditGovernanceOutboxRepo`（B5-1 handoff H3）：叶子 token 词表扩到 message/room 生命周期，同事务入队接线

- **Module (analysis root)**: `crates/aero-common/src`（AGENTS.md crate 地图叶子层）；交付面含 `crates/aero-ai/src/governance.rs`（lane 映射权威）、`crates/aero-storage/src/audit_governance.rs`（H3 落点）、`crates/aero-storage/src/message/*.rs` + `room*.rs`（生产者接线）、`scripts/truth-check-lib.sh`（token 单源守卫）
- **Direction**: "Land message.*/room.*/admin.* lane mapping + in-tx AuditGovernanceOutboxRepo (B5-1 handoff H3): extend governance_lane_for beyond message.moderated and wire same-transaction enqueue for message/room lifecycle producers"（value 9 / risk_reduction 9 / effort 8 / confidence 7）
- **Source analysis**: `docs/auto/analyses/crates-aero-common-src-630e5499.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`；gate anchor `docs/campaigns/implementation-gate.md:64`（"`message.*`/`room.*`/`admin.*` 同事务写入；‡ 类走 L1"）、`:78`（G6 = "37/37、T-11、moderation 优先级"）
- **Sibling specs（同批次，边界协调）**: `2026-08-08-aero-common-b5-contract-vocabulary-leaf-types.req.md`（词表已落地：status/class/envelope twin）、`2026-08-08-aero-auth-b5-1-in-tx-audit-outbox.req.md`（**共享 H3 落点**：`AuditGovernanceOutboxRepo` 只落一次，auth 切片与本节拍共同消费）、`2026-08-08-aero-ai-b5-3-token-parity-harness.req.md`、`2026-08-08-aero-cli-b5-3-moderation-priority-drill.req.md`（37-slot claim 排序 pin）
- **Status**: Requirements（下述证据全部经源码 grep + **实跑**核对：`cargo test -p aero-common` 3 passed/1 ignored、`cargo test -p aero-ai governance::` 6 passed，2026-08-08 实跑；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

> ⚠️ **与 analysis 的关键差异（§1.1 逐条更正）**：① 叶子已不止两个 token——sibling 词表方向已落 `AuditClaimPayload`/`AuditActor`/`AuditTarget` 16-key envelope twin 与 `AUDIT_*` 常量（生产者 payload 构建器已就绪，只缺 5 个本地 token）；② direction 说 "no message/room path writes status-0 rows in the same transaction" 需精确化：message **delete** 已 in-tx 写 audit 行（`soft_delete_audited` crud.rs:364），moderation finalize 已由 0239 触发器入 outbox——缺的是**非 moderation token 的 outbox 行** + **insert/edit/room 路径的 audit 行本身**；③ 生产者接线会改动 2 个已钉 db_tests（`non_moderation_action_passes_through_unmapped` / `governance_reconcile_backfills_disabled_window`）——§4 R7 枚举重写而非回归；④ 显式写路径的门语义 direction 未定义——本 spec 定为**无条件入队**（镜像 auth sibling R6，§4 R6）。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-common/src/model/audit.rs` — GOVERNANCE_CLASS_MESSAGE/ROOM/ADMIN 存在但仅 LOCAL_ACTION_MODERATED + MODERATION_OUTBOUND_ACTION 两个 token；"locked as ONE constant" 注释 | ✅ **Verified（行号 :150/:156/:164-168）**。`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` :150（doc :141-148 明言 "'admin.moderation.action' from the contract was deliberately collapsed into it…locked here as ONE constant"）；`LOCAL_ACTION_MODERATED = "message.moderated"` :156；`GOVERNANCE_CLASS_{ADMIN,MESSAGE,ROOM}` :164-168（派生自 `AuditClass::as_str()`）。**全仓仅这两个 action token 常量**（rg 确认）。**补充**：sibling 词表方向已落 `AuditClaimPayload`（:238 `new()` 生产者构造器，8 个可变入参 + 8 个 `AUDIT_*` 不变量）、`AuditActor`/`AuditTarget`、`OutboxStatus`/`AuditClass` 全套 serde pin 测试（:425-429 `vocabulary_consts_are_pinned`；:446 sample payload 已用 `"message.deleted"` 作示例 action）——**本 direction 不需要新 envelope 类型** |
| E2 | `crates/aero-ai/src/governance.rs` `governance_lane_for`（单臂 match，其余 token → None） | ✅ **Verified（:74-102）**。唯一 match 臂 = `LOCAL_ACTION_MODERATED` → `{class:'admin', priority:100, outbound:'admin.content.flag', status:0}`，`_ => None`。`GOVERNANCE_PRIORITY_{MODERATION=100,BACKLOG=10}` :31/:33；re-export 链 :41。单测 6 项实跑全绿（`cargo test -p aero-ai governance::`），其中 `unknown_local_token_passes_through_unmapped` :126 把 `room.create`/`message.create`/`message.edit`/`message.deleted`/`call.join`/`""` 全部断言为 None、`user_delete_token_stays_out_of_admin_lane` :152 钉 `message.deleted → None`（R-D2）——**这两个测试随本 direction 换形**（§4 R3） |
| E3 | `crates/aero-storage/src/audit_governance.rs:22`（H3 handoff marker） | ✅ **Verified（模块 doc :19-27，2026-08-08 实测）**。原文："The B5-1 storage direction's `AuditGovernanceOutboxRepo` lands in a sibling slice (handoff H3 of the 0239 design doc); this module carries the DDL-contract fixtures only — **add the repo to this same module when it lands**"。`rg AuditGovernanceOutboxRepo` 全仓唯一命中 = 该 doc——**repo 尚未落地** |
| E4 | `migrations/0239_audit_governance_outbox.sql`（trigger 仅分支 `NEW.action = 'message.moderated'`） | ✅ **Verified**。`aero_enqueue_governance_audit()`：Gate 1（runtime disabled → RETURN NEW）+ `IF NEW.action <> 'message.moderated' THEN RETURN NEW`（token-keyed pass-through）+ Gate 2（binding RAISE，moderation 行 fail-closed）+ `ON CONFLICT (event_id) DO NOTHING` 幂等。**SQL 侧 token 集 = 仅 moderation；本 direction 不改 DDL/trigger**（非 moderation token 全部走 Rust 显式写路） |
| E5 | `crates/aero-storage/src/audit.rs:154`（INSERT INTO audit_events pattern） | ✅ **Verified（:154-167 `append_on`；`append_in_tx` :127）**。`AuditRepo::append_in_tx(tx, workspace, actor, action, target, detail) -> Result<AuditId>`——返回的 `AuditId` 即 outbox 行 `event_id`（1:1 前提），`append_on` 的 INSERT 模式（:154）是本 direction in-tx 组合的复用点 |
| E6 | aero-im-core 零 audit_events producers（direction 陈述） | ✅ **Verified**。`rg -n "audit" crates/aero-im-core/src --glob '*.rs'` 仅注释命中（events.rs:5、messages.rs:533、pii_detect.rs:35）——零 audit 调用。**补充**：aero-im-core 依赖 aero-common + aero-storage（Cargo.toml:13-14），消费叶子常量与仓储组合均合法 |
| E7 | （补充）生产者 seam 盘点（message/room 生命周期现状） | ✅ **Verified**。① **message.create**：`MessageRepo::insert`（crud.rs:31，已 tx）· `insert_idempotent`（idempotency.rs:84）· `insert_outboxed`（idempotency.rs:158，**im-core 发送路径实际调用点** messages.rs:216）；共享 in-tx 内核 `insert_row_in_tx`（crud.rs:63）。② **message.edit**：`MessageRepo::edit`（crud.rs:168，**单语句非 tx**）· `edit_outboxed_authorized`（authorization.rs:403，im-core 编辑路径）。③ **message.deleted**：`soft_delete_audited`（crud.rs:364，已 tx + audit in-tx，**无 outbox 行**）· `soft_delete_outboxed_authorized`（authorization.rs:458，im-core 删除路径 messages.rs:447）；moderation 路径 `soft_delete_moderated`（crud.rs:398）/`soft_delete_outboxed_system`（events.rs:267，audit_action 参数化，im-core messages.rs:632）**已由 trigger 入 outbox，不得双写**。④ **room.create**：`RoomRepo::create_in_workspace`（room.rs:267，已 tx）· `create_in_workspace_authorized`（room/governance.rs:108，im-core 路径 room.rs:77）。⑤ **room.archived**：`set_channel_archived_authorized`（room/governance.rs:396，已 tx，im-core `archive_channel` channels.rs:123 → server `archive_channel` channels.rs:115）· 裸 `RoomRepo::archive`（room.rs:437，单语句非 tx）。**insert/edit/room 路径今天零 audit 行** |
| E8 | （补充）claim 排序契约 | ✅ **Verified**。`claim_due` ORDER BY = `priority DESC, available_at, created_at, event_id`（connector pg.rs:117）；priority drill `moderation-in-first-batch`（bin :283）断言 500 条 priority-10 backlog + 1 条 priority-100 moderation 同批可领时 moderation 必在前 100 内（"claim order must be priority DESC, not FIFO"）；b5-pin.sh:41 钉 `moderation-priority-drill` 槽（37-slot 清单，`scripts/test-b5-pin-guard.sh` 37/37 必须绿） |
| E9 | （补充）Rust 产 message 行通过 0239 CHECK 的先例 | ✅ **Verified**。`rust_produced_payload_matches_0239_envelope`（audit_governance.rs:445 起，half B）：直接 SQL INSERT class 'message'/priority 10 + `AuditClaimPayload::new` 信封，0239 CHECKs 全接受、读回逐字段断言全过——**新车道的 payload 形态已被 drill 证明合法** |
| E10 | （补充）harness 门 | ✅ **Verified**。`scripts/test-integration.sh:313-323`：`audit_governance::` + `moderation_finalize_outbox_parity` 两个 `run_migrated_integration` 条目（`-p aero-storage --lib`，0239 文件已存在 → **un-gated**；空过滤器守卫禁 vacuous green）——本 direction 新 db_tests 落同一模块即自动覆盖，**零 harness 改动** |
| E11 | （补充）R-D2 / T-11 既有 pin | ✅ **Verified**。governance.rs `user_delete_token_stays_out_of_admin_lane` :151-156（`message.deleted` 永不进 admin lane）；`non_moderation_action_passes_through_unmapped`（audit_governance.rs:818，trigger 对非 moderation action 零 outbox 行 + v1 行照产）；`governance_reconcile_backfills_disabled_window`（:964 起，0241 reconciler 对 unmapped token 永不 fabricate admin 行）；T-11 drill（`aero-audit-t11-drill.rs`：relay 缺席 → 行保持 pending，不假成功不假死） |

### 1.1 对 direction 陈述的勘误/钉化（evidence-backed）

- **「audit.rs 只定义两个 action token」准确但不完整**：两个 token 之外，叶子已含完整 16-key envelope twin（E1）——生产者的 payload 构建器（`AuditClaimPayload::new`）已存在并被 half-B drill 使用（E9），本 direction 的叶子交付 = **5 个新本地 token + 守卫**，不是新类型。
- **「no message send/edit/delete or room create/archive path writes status-0 rows in the same transaction」需精确化**：message **delete** 已 in-tx 写 audit 行（`soft_delete_audited` crud.rs:364/374）；moderation finalize 已由 0239 触发器 in-tx 入 outbox（E4）。缺口 = ① `message.deleted` 等非 moderation token 的 outbox 行（delete 路径只有 audit 行）；② `message.create`/`message.edit`/`room.create`/`room.archived` 路径**连 audit 行都没有**（E7）——所以验收的「audit row AND status-0 outbox row 原子提交」对后四者意味着同时新增 audit 行与 outbox 行。
- **R-D2 换形而非弱化**：`message.deleted` 从「None（完全 unmapped）」改为「class 'message'、priority 10」——R-D2 的核心不变量是**永不进 admin lane**（governance.rs:151-156 的 `!is_admin_class("message.deleted")` 断言保留），「进 message backlog lane」与 R-D2 兼容；pass-through 负例由仍 unmapped 的 token（`call.join`/`message.recalled`/`""`）承接。
- **生产者接线触碰 2 个已钉 db_tests**（非回归，是语义更新）：`non_moderation_action_passes_through_unmapped`（:818）与 `governance_reconcile_backfills_disabled_window`（:964）当前断言 `soft_delete_audited` 后零 outbox 行——接线后该路径产 1 行 message-class 行；两测试改走「直接 INSERT audit_events + unmapped token」隔离 trigger pass-through（§4 R7 逐条枚举）。`moderation_finalize_outbox_parity`（:251）名称/字面量**不动**（harness 过滤器钉死）。
- **显式写路径的门语义 direction 未定义**：本 spec 钉为**无条件入队**（不 consult `snaplink_commercial_runtime.enabled`、无 binding RAISE、无 Gate 2）——镜像 auth sibling R6（账户/内容事件不因商业开关被抑制），且避免在 Rust 复制 SQL gate 机制（消息发送不因缺 binding 失败）；**0241 reconciler 保持 token-keyed 于 message.moderated，零 SQL 改动**（disabled-window 自愈仍是 moderation trigger 路专属语义，E4）。
- **claim 测试落点**：aero-storage 不得依赖 aero-audit-connector（依赖方向）——claim-order db_test 在 aero-storage 内镜像 pg.rs 的 claim SQL（文本 cross-pin 注释，仓库既有惯例：audit_governance.rs 模块 doc 明言 leaf↔DDL 靠「literal + behavioral tests」互钉）；pinned priority drill（37-slot）**不动**。

## 2. Verified current state（实跑证据）

```
基线（本 spec 亲自实跑，2026-08-08）：
  cargo test -p aero-common            → 3 passed; 1 ignored（叶子词表/envelope pin 全绿）
  cargo test -p aero-ai governance::   → 6 passed（governance_lane_for 单臂映射 + R-D2 pin 全绿）

入队现状（两条路）：
a) 触发器路（E4）  audit_events AFTER INSERT → aero_enqueue_governance_audit()
                   └─ 仅 action='message.moderated' → outbox（class admin / priority 100）
                     其余 token → RETURN NEW pass-through（fail-open 不 raise）
b) 显式写路（E3）  AuditGovernanceOutboxRepo —— 尚不存在（H3 手记：落 audit_governance.rs 同模块）

message/room 生命周期审计现状（E7，全部 verified）：
  message.create → 零 audit 行、零 outbox 行（insert crud:31 / insert_idempotent :84 /
                   insert_outboxed :158 = im-core 发送路径 messages.rs:216）
  message.edit   → 零 audit 行、零 outbox 行（edit crud:168 / edit_outboxed_authorized authorization:403）
  message.deleted→ audit 行 in-tx（soft_delete_audited crud:364 / soft_delete_outboxed_authorized
                   authorization:458）；outbox 行 = 0（trigger 不认 message.deleted）
  room.create    → 零 audit 行、零 outbox 行（create_in_workspace room:267 /
                   create_in_workspace_authorized room/governance:108）
  room.archived  → 零 audit 行、零 outbox 行（set_channel_archived_authorized room/governance:396 /
                   archive room:437）
  message.moderated → audit in-tx + outbox in-tx（trigger 路，已闭环，不得双写）
```

**Gaps this direction closes**（all verified）：① 叶子缺 5 个本地 token（E1）且无 token 单源守卫（truth-check 现仅钉 `admin.content.flag`）；② `governance_lane_for` 单臂（E2），message/room 车道未映射；③ `AuditGovernanceOutboxRepo` 不存在（E3）；④ message insert/edit + room create/archive 无 audit 行、delete 无 outbox 行（E7）；⑤ 新车道排序无 claim 级测试（E8 只覆盖 message-class backlog 与 moderation）。

## 3. Scope

**In scope**：
- `crates/aero-common/src/model/audit.rs`：5 个新本地 action token 常量（R1）+ 规范值 pin 测试（R2）
- `scripts/truth-check-lib.sh`：token 单源守卫扩到新 token 集（R2；rg=0 于 audit.rs 之外）
- `crates/aero-ai/src/governance.rs`：`governance_lane_for` 新增 5 臂 + 受影响单测换形（R3）
- `crates/aero-storage/src/audit_governance.rs`：`AuditGovernanceOutboxRepo`（H3 落点，R4）+ per-token parity db_tests（AC1/AC2）+ claim-order db_test（AC4）+ 2 个既有 db_test 重写（R7）+ 生产字面量清理（R5/R2）
- `crates/aero-storage/src/message/*.rs` + `room*.rs`：5 条生产者 seam 的 in-tx audit+outbox 接线（R5）
- 门禁保持：`audit_governance::` / `moderation_finalize_outbox_parity` harness 条目（零改动）、b5-pin 37/37、priority drill 不动（R8）

**Out of scope（红线，不扩）**：
- **不改 0239/0240/0241 DDL 与触发器**（token 集、CHECK、默认值逐字保持；非 moderation token 全部走 Rust 显式写路）；**0241 reconciler 零改动**（仍仅 message.moderated 自愈）
- **`message.recalled`（0238 recall）**——不在 direction 的 token 集，保持 unmapped pass-through（兼作 R-D2/T-11 负例）
- **`room.unarchive`（archived=false 方向）**——direction 只点 room create/archive；unarchive 不新增 token
- **auth 域 token（auth.register 等）**——auth sibling 切片范围，共享 H3 repo 但 token/接线各自独立
- **L1 聚合、delivery_mode 策略（B5-3）**——[PROPOSED]，本 direction 只产逐事件 1:1 行
- **v1 `snaplink_delivery_outbox` 路径、`governance_lane_for` 的 fail-closed 化（unmapped 改 raise）**——R-D2 明令禁止第二 abort 路径
- **`message.moderated` 字面量清理**——既有 shipped 状态，非本 direction 的「新 token」；守卫覆盖范围 = 5 个新 token（对称扩展 message.moderated 留作可选后续）

## 4. Requirements

### R1 — 叶子新增本地 action token 词表（`crates/aero-common/src/model/audit.rs`）

新增 5 个常量（值 = 既有生产字面量/映射测试候选串，逐字 verified，E2/E7）：

| 常量 | 值 | 车道 class |
|---|---|---|
| `LOCAL_ACTION_MESSAGE_CREATE` | `"message.create"` | message |
| `LOCAL_ACTION_MESSAGE_EDIT` | `"message.edit"` | message |
| `LOCAL_ACTION_MESSAGE_DELETE` | `"message.deleted"` | message |
| `LOCAL_ACTION_ROOM_CREATE` | `"room.create"` | room |
| `LOCAL_ACTION_ROOM_ARCHIVE` | `"room.archived"` | room |

- 与既有 `LOCAL_ACTION_MODERATED` 同区块（action-token vocabulary 区，:149-156 后），doc 注明「本地审计 token，生产调用点一律经常量拼写，绝不裸字面量」。
- **不新增出站契约 token**：message/room 车道的 payload `action` = 本地 token 原样（half-B drill 先例：`back.action == "message.deleted"`，E9；v1 路径同样保留本地 token）。`MODERATION_OUTBOUND_ACTION` 仍是唯一出站契约 token。
- **不新增 envelope 类型**：`AuditClaimPayload::new`（:238）即生产者构建器。

### R2 — 叶子 pin 测试 + truth-check token 单源守卫

- `#[cfg(test)]` 内新增 pin：5 个新常量规范值断言（镜像 `vocabulary_consts_are_pinned` :425-429 形态）；`cargo test -p aero-common` 基线（3 passed/1 ignored）不回归。
- `scripts/truth-check-lib.sh`：把 AUDIT-FLAG 守卫（现仅 `admin.content.flag`）扩为 **token 集扫描**——对 5 个新 token 逐个 `rg -n -F '"<token>"' crates --glob '*.rs'`，命中须 ∈ allowlist（叶子定义文件 `CLAIM_AUDIT_FILE` 豁免 + 明确登记的第二站点，若有）；**任何 audit.rs 之外的裸字面量 = 违规计入 exit 码**（F5：`exit orphan + guard`）。
- 清理面（本 direction 落地时随 token 定义一并切换，全 verified）：`aero-storage` 生产字面量 `crud.rs:378`（soft_delete_audited 的 audit action）、`events.rs:622`（soft_delete_outboxed_system 测试调用点属 test，但 events.rs 生产路径的 audit_action 传参点以常量拼写）、`authorization.rs:503`；governance.rs 单测字面量（R3 换形时一并改常量）；aero-storage db_tests 字面量（audit_governance.rs:511/:534/:585/:841、audit.rs:737/:761/:837/:862）。SQL（0239/0241 的 `'message.moderated'`）不在扫描面（非 .rs）。

### R3 — `governance_lane_for` 扩展到 5 个新 token（`crates/aero-ai/src/governance.rs`）

- 新增 5 个 match 臂：`LOCAL_ACTION_MESSAGE_CREATE/EDIT/DELETE` → `{class: GOVERNANCE_CLASS_MESSAGE, priority: GOVERNANCE_PRIORITY_BACKLOG(10), outbound_action: <本地 token 自身>, status: 0}`；`LOCAL_ACTION_ROOM_CREATE/ARCHIVE` → 同形但 `class: GOVERNANCE_CLASS_ROOM`。**admin 车道保持仅 `message.moderated`**（`is_admin_class` 语义不变：5 个新 token 全部 `false`）。
- 单测换形（**保留断言意图，改断言形式**）：
  - `unknown_local_token_passes_through_unmapped` :126：列表去掉 4 个已映射 token，保留负例集 = `["call.join", "message.recalled", ""]`（`message.recalled` 兼钉 recall 不在本方向 token 集）；`is_admin_class` 负断言保留。
  - `user_delete_token_stays_out_of_admin_lane` :152：`assert_eq!(governance_lane_for("message.deleted"), None)` 改为 `Some(lane)` 且 `lane.class == GOVERNANCE_CLASS_MESSAGE` + `!is_admin_class("message.deleted")`——**R-D2 不变量（永不 admin）以更强形式保留**；doc 注释更新（原「produce no outbound token」表述已不适用：message lane 的 outbound = 本地 token）。
  - `mapping_is_token_keyed` :163 / `admin_class_rows_never_aggregated` :195：现有断言（5 个 token 非 admin-class）**原样通过**；doc 微调（"message.moderated is the only admin-class token today" 保持真）。
  - 新增：per-token 映射 pin 测试（每个新 token → 正确的 class/priority=10/outbound=自身/status=0，DESC 语义注释同 `moderation_lane_preempts_backlog_under_desc_claim`）。
- 既有 6 单测全绿基线 + 新增测试 = 本模块最终 ≥11 项（数量锚点仅作参考，验收以全绿为不变式）。

### R4 — `AuditGovernanceOutboxRepo`（H3 落点，`crates/aero-storage/src/audit_governance.rs`）

```rust
/// H3 交付：v2 出站仓储（0239 表）。event_id = audit_events.id（1:1）。
impl AuditGovernanceOutboxRepo {
    pub fn new(pool: PgPool) -> Self;
    /// 事务内写 outbox 行（镜像 AuditRepo::append_in_tx 形态，audit.rs:127）。
    /// 0239 PK 自带 ON CONFLICT 语义由调用方显式 DO NOTHING 或依赖 PK 违例报错——
    /// 本方法执行 INSERT … ON CONFLICT (event_id) DO NOTHING（重放幂等）。
    pub async fn append_in_tx(
        tx: &mut Transaction<'_, Postgres>,
        event_id: AuditId,
        class: &str,          // GOVERNANCE_CLASS_*（0239 CHECK 校验）
        priority: i16,        // GOVERNANCE_PRIORITY_*（0239 CHECK > 0）
        payload: serde_json::Value,  // AuditClaimPayload::new 产物（jsonb CHECK = object）
    ) -> Result<(), sqlx::Error>;
}
```

- **共享落点纪律**：本 repo 全仓只落一次（H3 手记明示同模块）；auth sibling 的 `append_pair_in_tx_fail_open`/`record_pair_standalone` 消费同一 repo（其 SAVEPOINT fail-open 语义属 auth 切片，本 direction 不实现——message/room 车道走同事务 fail-closed，见 R5/R6）。
- 行字段契约（镜像 moderation parity 的 Half 3 断言，E4/E9）：`status=0`、`class`/`priority` 由入参（0239 CHECK 值域内）、`payload` 含 `idempotency_key = event_id::text` 等 16-key 信封（`AuditClaimPayload::new` 强制）。
- `source_system` 规则：workspace 有 enabled binding 时取其 `source_system`（parity fixture `enable_enforcement_with_binding` 先例），否则回退常量 `"aero-im"`（与叶子 sample payload 同源；**无 binding 不 raise、不入商业门**）。

### R5 — 生产者接线：message/room 生命周期同事务 audit + outbox（`crates/aero-storage`）

统一模式：每条生产者 seam 在其既有事务内追加「① 域变更 → ② `AuditRepo::append_in_tx`（返回 AuditId）→ ③ `AuditGovernanceOutboxRepo::append_in_tx`（event_id = ② 的 AuditId）」；**任一失败 → 整事务回滚（0 行逃逸）**。token 经叶子常量拼写（R2 守卫）。

| 本地 token | 生产者 seam（已 verified 调用点） | 接线要点 |
|---|---|---|
| `message.create` | `MessageRepo::insert`（crud.rs:31）· `insert_idempotent`（idempotency.rs:84）· `insert_outboxed`（idempotency.rs:158，im-core 发送路径） | 共享 in-tx 组合（`insert_row_in_tx` 内核 + workspace 解析 `rooms.workspace_id`）；actor = sender；target = message id |
| `message.edit` | `MessageRepo::edit`（crud.rs:168）· `edit_outboxed_authorized`（authorization.rs:403） | edit 现为单语句 → 包事务；actor = 编辑者（入参）；target = message id；version 冲突路径不写 audit（无域变更 = 无审计行） |
| `message.deleted` | `soft_delete_audited`（crud.rs:364）· `soft_delete_outboxed_authorized`（authorization.rs:458，im-core 删除路径 messages.rs:447） | audit 已 in-tx，补 outbox 行；已删除/缺失消息（`Ok(false)`）不写（parity replay 半部语义） |
| `room.create` | `RoomRepo::create_in_workspace`（room.rs:267）· `create_in_workspace_authorized`（room/governance.rs:108） | 已 tx；actor = created_by；target = room id；`workspace` 即入参 |
| `room.archived` | `set_channel_archived_authorized`（room/governance.rs:396，im-core `archive_channel`）· 裸 `RoomRepo::archive`（room.rs:437） | archived=true 写 `room.archived`；archived=false（unarchive）**不写**（出范围）；裸 archive 单语句 → 包事务 + workspace 解析 |

**moderation 路径不得双写**：`soft_delete_moderated`（crud.rs:398）/`soft_delete_outboxed_system`（events.rs:267，audit_action=`message.moderated` 时）不显式入队——0239 触发器是该 token 的**唯一** outbox 生产者（显式写会撞 PK 走 DO NOTHING 无害，但语义上触发器独占，R5 硬线）。`soft_delete_outboxed_system` 的 audit_action 参数化：调用方传 `message.deleted` 时按 message 车道显式入队、传 `message.moderated` 时跳过（分支判据 = `governance_lane_for(audit_action)` + `audit_action != LOCAL_ACTION_MODERATED`）。

### R6 — 显式写路径门语义（本 spec 钉定，direction 未定义）

- **无条件入队**：message/room 车道 outbox 行不 consult `snaplink_commercial_runtime.enabled`、不做 binding RAISE（Gate 1/Gate 2 语义属 SQL trigger 路，Rust 显式写不复制）——消息发送/编辑/删除与房间生命周期不因商业开关或缺 binding 失败；`source_system` 回退规则见 R4。
- **同事务 fail-closed**：audit/outbox 写失败 = 域操作失败（整事务回滚，无半对）——镜像 ROADMAP 方向五「审计事务化」既有语义（crud.rs:359-363 doc：「a message is never silently deleted unaudited」）与 moderation trigger 的 Gate 2 RAISE 语义。auth sibling 的 SAVEPOINT fail-open 是 auth 切片的 UX 决策，不适用本车道（§7 D1 记录分歧）。
- **0241 reconciler 零改动**：disabled-window 自愈保持 moderation-only；producer 行无条件写，故无自愈需求。

### R7 — 既有 db_tests 的受控换形（非回归，语义更新；逐条枚举）

- **`non_moderation_action_passes_through_unmapped`**（audit_governance.rs:818）：改为**直接 `INSERT INTO audit_events`**（不经生产者 seam，镜像 `duplicate_event_id_is_deduped_by_on_conflict` :883 的直插形态）+ **仍 unmapped 的 token**（`'call.join'` 或 `'message.recalled'`）→ 断言 0 outbox 行 + v1 行照产——**纯 trigger pass-through 断言，与生产者路径解耦**。测试名保留（不在 harness 命名钉位，但语义连续性更好）；doc 更新。
- **`governance_reconcile_backfills_disabled_window`**（:964 起）：id2 的 `soft_delete_audited` 现产 1 行 message-class producer 行（无条件，R6）→ ① ws-scoped 断言从 `governance_rows_for == 1` 改 `== 2`（1 行 admin/100 moderation backfill + 1 行 message/10 producer）；② `fetch_one`（`WHERE payload->>'aggregate_id' = $1`）改 `fetch_all` + 按 `payload->>'action'`/`class` 逐行断言（admin 行 action=`admin.content.flag`、producer 行 action=`message.deleted`）——**「unmapped token 永不 fabricate admin 行」的 R-D2 断言保留且更强**；③ 「dead 不复活」半部不变。
- **`moderation_finalize_outbox_parity` / `ddl_contract_defaults_and_checks` / `moderation_finalize_runtime_disabled_commits_1_plus_0` / `duplicate_event_id_is_deduped_by_on_conflict` / `rust_produced_payload_matches_0239_envelope`**：**名称与字面量不动**（harness 钉位 + 契约 fixture），仅在需要处把裸 token 字面量换叶子常量（R2 清理面）。
- **`audit.rs` 既有 db_tests**（:737/:761/:837/:862 的 `"message.deleted"`/`"message.moderated"` 断言字面量）：换叶子常量，断言语义不变。

### R8 — claim-order db_test（新车道 DESC 排序）+ 既有 pin 保持

- 新增 db_test（`audit_governance.rs` 同模块，`#[ignore = "requires live Postgres"]`）：种子 = 1 行 moderation（class 'admin'、priority 100）+ 5 行新车道（message×3 / room×2，priority 10，payload action = 各自本地 token），**backlog 先种（available_at 更早）、moderation 后种**（镜像 priority drill 种子纪律，任何排序证据必须来自 priority）；以 **pg.rs `claim_due` 逐字镜像 SQL**（ORDER BY `priority DESC, available_at, created_at, event_id`，cross-pin 注释：「must equal crates/aero-audit-connector/src/pg.rs:117 claim_due ORDER BY」）执行：`LIMIT 1` 首领 = moderation 行；随后 `LIMIT 5` 领走全部 5 行 backlog（集合相等）——**新 backlog 车道严格排在 moderation 车道之后（DESC）**。
- **b5-pin 37/37 保持**：`moderation-priority-drill`（b5-pin.sh:41）与其 3 条 PASS 断言**零改动**（其 `moderation-in-first-batch` 已证明 10-vs-100 排序，新测试扩展车道集）；`scripts/test-b5-pin-guard.sh` 37/37 绿；harness `audit_governance::` 条目自动覆盖新 db_tests（E10，零 harness 改动）。

## 5. Acceptance（direction 验收逐条保留 + 可测试化）

> direction 的 4 组验收全部保留，逐条给可执行形态。AC1/AC2/AC4 为 PG 门控 db_tests（`#[ignore = "requires live Postgres"]` + `DATABASE_URL` + throwaway 已迁移库，AGENTS.md §4.3），AC3 为脚本守卫，AC5 为全仓门禁。

- **AC1（per-token parity，in-tx oracle）**：5 个新 token 各一个 parity db_test（`message_create_outbox_parity` / `message_edit_outbox_parity` / `message_delete_outbox_parity` / `room_create_outbox_parity` / `room_archive_outbox_parity`），**`moderation_finalize_outbox_parity` 形状**：提交半部——驱动真实生产者 seam 后，`audit_events` 恰 1 行（action = 本地 token 原样、actor/target 正确）+ `audit_governance_outbox` 恰 1 行（`status = 0`、`class` = message/room、`priority = 10`、`event_id` = 该 audit 行 id 1:1、`payload->>'action'` = 本地 token、`payload->>'idempotency_key'` = event_id、`source_system` = binding 值或 `"aero-im"`）；重放半部（delete 二次执行 / insert 幂等路径）不增行。
- **AC2（same-transaction proof）**：每个 parity 测试的**回滚半部**——注入失败后，域变更行 + audit 行 + outbox 行**三者俱无**（0 行逃逸）。注入点（逐 token 可执行）：`message.deleted` = 传入不存在的 workspace（audit FK 违例，先例 events.rs `audit_failure_rolls_back_delete_and_outbox_append`）；`message.create`/`message.edit`/`room.create`/`room.archived` = 测试自开事务按 seam 的 in-tx 语句序列（域语句 + `append_in_tx`(audit) + `append_in_tx`(outbox)）执行，末步以**毒 payload**（`json!(42)`，违反 0239 `CHECK (jsonb_typeof(payload) = 'object')`）注入 → 断言 Err 且域行/audit 行/outbox 行全为 0（经 `ROLLBACK` 或自然回滚后查库）。断言形态：域表（messages/rooms）0 行 + `audit_events` 0 行 + `audit_governance_outbox` 0 行（ws-scoped）。
- **AC3（叶子 token 单源）**：`rg -n -F '"message.create"' / '"message.edit"' / '"message.deleted"' / '"room.create"' / '"room.archived"' crates --glob '*.rs'` 在 `crates/aero-common/src/model/audit.rs` 之外 **返回 0**（truth-check 守卫扩展后 `scripts/truth-check.sh` 0 违规，R2 清理面全部落地；SQL 侧不在扫描面）。
- **AC4（claim 排序）**：R8 的 claim-order db_test 绿——`LIMIT 1` 首领 = moderation（priority 100）行，`LIMIT 5` 领走全部 5 个新车道行（priority 10，message/room）——**新 backlog 车道严格排在 moderation 车道之后（DESC）**；`scripts/test-b5-pin-guard.sh` **37/37 绿**（`moderation-priority-drill` 槽 PASS 不回归，b5-pin.sh:41 不动）。
- **AC5（T-11 fail-closed / R-D2 回归）**：unmapped/token-less 行**永不 fabricate outbox 行**——① 单测：`governance_lane_for("call.join") == None`、`governance_lane_for("message.recalled") == None`、`is_admin_class` 对 5 个新 token 全 false（R3 换形后保留）；② db_test：直接 INSERT 的 unmapped audit 行（`'call.join'`）经 trigger 零 outbox 行（R7 重写后的 `non_moderation_action_passes_through_unmapped`）；③ reconciler 测试：disabled-window 的 unmapped 行永不被 fabricate 成 admin 行（R7 重写后的 `governance_reconcile_backfills_disabled_window` 的按行 class/action 断言）。

## 6. Test placement

| 测试 | 位置 | 门 |
|---|---|---|
| 5 个新 token 规范值 pin | `crates/aero-common/src/model/audit.rs` `#[cfg(test)]` | `cargo test -p aero-common`（基线 3+1 不回归） |
| lane 映射（5 臂 + 负例 + R-D2 换形 + per-token pin） | `crates/aero-ai/src/governance.rs` `#[cfg(test)]` | `cargo test -p aero-ai governance::` |
| `AuditGovernanceOutboxRepo` 单元面（无 DB：签名/文档） | `audit_governance.rs` 模块层 | `cargo check --workspace` |
| per-token parity ×5（AC1/AC2）· claim-order（AC4）· pass-through/reconcile 重写（AC5） | `crates/aero-storage/src/audit_governance.rs` `db_tests` | harness 既有条目 `audit_governance::`（`-p aero-storage --lib` + `-- --ignored` + throwaway 已迁移库）——**零 harness 改动** |
| token 单源扫描 | 脚本守卫 | `scripts/truth-check.sh`（扩展后）+ `rg` 命令（AC3） |
| b5-pin | 既有 | `scripts/test-b5-pin-guard.sh` 37/37 + `scripts/test-integration.sh` B5 段（含 `moderation_finalize_outbox_parity` 钉位不动） |
| 生产者 seam 行为（audit 失败回滚先例） | `crates/aero-storage/src/message/events.rs` 既有测试 | 不回归（`soft_delete_outboxed_system` 路径语义不变，仅 audit_action=`message.deleted` 时新增入队） |

## 7. Risks / [PROPOSED] / 决策点

- **D1（fail-open vs fail-closed 分歧）**：auth sibling R7 对 auth 事件选 SAVEPOINT fail-open（审计失败不翻转操作）；本 direction 选同事务 fail-closed（审计是内容域操作的 load-bearing 轨迹，ROADMAP 方向五先例 + moderation Gate 2 语义一致）。风险：message send 因 audit 写失败而失败（低概率：FK/CHECK 违例只可能由 bug 引起）——记录为有意的语义分歧，不改。
- **D2（`message.deleted` 从 None → message lane 的语义后果）**：v1 路径（0236 trigger）与 0241 reconciler 均 token-keyed 于各自 SQL 字面量，不受 Rust 映射扩展影响（SQL 不动）；`non_moderation_action_passes_through_unmapped` 与 reconcile 测试的换形（R7）是**唯一**既有测试面——已逐条枚举，无隐藏面（其余 4 个钉位测试经核对不受影响）。
- **D3（高容量 message.create 逐事件一行）**：`message.create` 每消息一行 = 高容量 message lane 的既定形态（`GOVERNANCE_CLASS_MESSAGE` doc 明言 "High-volume message backlog class (L1-aggregatable)"）；L1 聚合（[PROPOSED]，B5-3）负责后续合并，本 direction 只产 1:1 行。
- **Risks**：① 0239-0241、governance.rs、audit_governance.rs、aero-audit-connector、b5-pin.sh 等 B5 基底文件仍 untracked——本 direction 落地前须先提交上一批次（sibling verified-landing 的 Oracle 已列此义务），否则 `git reset --hard` 校准丢地基；② edit/裸 archive 需包事务（现为单语句）——行为面 = 版本冲突/竞态语义不变（原单语句原子性由语句自身保证，包事务后相同），但 `soft_delete_audited` 类方法的并发先例（audit.rs db_tests）须全绿；③ truth-check 守卫扩展到 token 集可能误伤既有 test 字面量（audit_governance.rs 等）——R2 清理面已枚举全部 verified 命中点，无未列站点；④ `[PROPOSED]`（不在本 direction）：L1 聚合、delivery_mode 策略、`message.moderated` 字面量守卫对称扩展、auth 域 token 接线。

## 8. Sequencing / verification

1. **前置**：提交上一批次 untracked B5 基底（0239/0240/0241、audit_governance.rs、governance.rs、aero-audit-connector、drills、b5-pin.sh 等）——AC3/AC4 的基线前提。
2. R1+R2：叶子 5 token + pin 测试 + truth-check 守卫扩展 + 全仓字面量清理 → `cargo test -p aero-common` 先绿；`rg` 5 token 扫描 = 0（AC3）。
3. R3：governance.rs 5 臂 + 单测换形 → `cargo test -p aero-ai governance::`（AC5 ①）。
4. R4：`AuditGovernanceOutboxRepo` 落 `audit_governance.rs` → `cargo check --workspace` 干净。
5. R5+R6：5 条生产者 seam 接线（message create/edit/delete、room create/archive；moderation 不双写）→ `cargo test --workspace --lib`（PG 门控 `-- --ignored`）。
6. R7+R8：per-token parity ×5（AC1/AC2）+ claim-order（AC4）+ 2 个既有 db_test 换形（AC5 ②③）→ throwaway 库跑 `audit_governance::` 全绿；`moderation_finalize_outbox_parity` 钉位零改动。
7. 收尾门禁：`cargo clippy --workspace --all-targets`（不新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `scripts/test-b5-pin-guard.sh` 37/37 · `scripts/test-integration.sh` B5 段全 PASS（AC4/AC5 终验）。
