# Requirements Spec — aero-im-core B5-1：message.*/room.*/admin.* 写路径同事务审计 + 治理 outbox 入队（服务边界 oracle 腿）

- **Module (analysis root)**: `crates/aero-im-core` — 服务层写路径（`service/messages.rs`、`service/room.rs`、`service/channels.rs`、`service/pins.rs`）；交付面含 `crates/aero-storage` 的 tx seam 接线（依赖方，见 §3）与 `crates/aero-im-core/src/db_tests/audit_governance_tests.rs`（新测试文件）
- **Direction**: "Complete in-tx audit writes + governance-outbox enqueue for message.*/room.*/admin.* operations in the aero-im-core write paths"（value 9 / risk_reduction 9 / effort 7 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-im-core-cfe64e80.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`；gate anchor `docs/campaigns/implementation-gate.md`（"`message.*`/`room.*`/`admin.*` 同事务写入"；G6 = "37/37、T-11、moderation 优先级"）
- **Sibling specs（同批次，边界协调）**: `2026-08-08-aero-common-b5-1-lane-mapping-in-tx-outbox.req.md`（**共享基础设施腿**：叶子 token 词表 + `governance_lane_for` 扩展 + `AuditGovernanceOutboxRepo`（H3）+ 5 条生产者 seam 接线 + 无条件入队门语义——本 spec 消费其交付物，**不重复规定**）、`2026-08-08-aero-common-b5-contract-vocabulary-leaf-types.req.md`（16-key envelope twin 已落地）、`2026-08-08-aero-eng-b5-1-operation-class-coverage-leg.req.md`（CLI 观察面腿 B3，种子 token `message.create`/`room.create`/`admin.content.flag` 与本 spec 词表一致）、`2026-08-08-aero-ai-b5-1-migration-0239-governance-outbox.req.md`（0239 落地 spec）
- **Status**: Requirements（下述证据全部经源码 grep 核对，2026-08-08；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）

> ⚠️ **direction 两处事实性更正**（§1.1 逐条）——① `delete_message` 并非「无审计」：`soft_delete_outboxed_authorized`（message/authorization.rs:458）已向 `soft_delete_locked_outboxed_in_tx` 传 `Some("message.deleted")`，in-tx 审计行**已存在**；缺的是 outbox 行。② `recall_message` 同理：`recall_locked_outboxed_in_tx`（authorization.rs:296）已 in-tx 写 `message.recalled` 审计行；缺 outbox 行。真正的零审计缺口 = `send_message`、`edit_message` 与全部 room.*/admin.* 治理操作（§1.1 钉化）。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `service/messages.rs` send/edit/delete/recall/moderate_delete | ✅ **Verified**。`send_message_inner` → `messages.insert_outboxed`（:216）；`edit_message` → `edit_outboxed_authorized`；`delete_message` → `soft_delete_outboxed_authorized`（:447）；`recall_message` → `recall_outboxed_authorized`；`moderate_delete` → `soft_delete_outboxed_system(..., Some("message.moderated"), ...)`（:632）。**全文件零 `AuditRepo`/`audit_events` 引用** |
| E2 | `service/room.rs`、`service/channels.rs`、`service/pins.rs` | ✅ **Verified**。`create_room_in_workspace` → `create_in_workspace_authorized`（room.rs:77）；`add_member` → `add_member_authorized`；`archive_channel` → `set_channel_archived_authorized`（channels.rs:123）；`set_channel_meta` → `patch_channel_authorized`；`set_room_post_policy` → `set_channel_post_policy_authorized`；slowmode/reaction-limit/retention → 同名 `*_authorized`；`pin_message`/`unpin_message` → `PinRepo::pin_authorized`/`unpin_authorized`（pins.rs）。**全文件零审计引用** |
| E3 | `crates/aero-storage/src/message/events.rs:336-344`（条件 `append_in_tx`） | ✅ **Verified**。`soft_delete_locked_outboxed_in_tx` :333-344：`if let (Some(workspace), Some(action)) = (workspace, audit_action) { AuditRepo::append_in_tx(...) }`——**条件追加的既有形态**（本 direction 的接线模板）。direction 断言「soft_delete_outboxed_authorized passes no action」**不成立**：authorization.rs:470-475 传 `Some(access.workspace), Some(actor), Some("message.deleted")`（§1.1 更正①） |
| E4 | `crates/aero-storage/src/message/crud.rs:374/407`（message.deleted / message.moderated 先例） | ✅ **Verified（行号轻微漂移）**。`soft_delete_audited`（:364-383，`"message.deleted"` + `append_in_tx`，doc：「a message is never silently deleted unaudited」）；`soft_delete_moderated`（:398-417，`"message.moderated"`）。**两者均无 outbox 行** |
| E5 | `migrations/0239_audit_governance_outbox.sql`（trigger 仅映射 `message.moderated`） | ✅ **Verified**。`aero_enqueue_governance_audit()`：Gate 1（runtime disabled → RETURN NEW）→ token 门（`NEW.action <> 'message.moderated'` → RETURN NEW，**fail-open pass-through 零 RAISE**）→ Gate 2（binding RAISE 仅 moderation 行）→ INSERT class='admin'/priority=100 + `ON CONFLICT (event_id) DO NOTHING`。DDL CHECKs：`class IN ('admin','message','room')`、`priority > 0`、`delivery_mode IN ('push')`、`status IN (0,1,2,3)`、`jsonb_typeof(payload)='object'` |
| E6 | `crates/aero-storage/src/audit.rs`（`AuditRepo::append_in_tx`） | ✅ **Verified（:127-136）**。签名 `append_in_tx(tx, workspace, actor: Option<ParticipantId>, action, target: Option<&str>, detail) -> Result<AuditId>`——**返回的 AuditId 即 outbox 行 event_id（1:1 前提）**；`append_on` INSERT 模式 :154-167 |
| E7 | `crates/aero-common/src/model/audit.rs`（LOCAL_ACTION_MODERATED） | ✅ **Verified**。`LOCAL_ACTION_MODERATED = "message.moderated"` :156；`AuditClass`/`OutboxStatus`/`GOVERNANCE_CLASS_*` :129-168；16-key envelope twin `AuditClaimPayload::new` :268-320（doc 明言「Consumers are *producers* of `message.*`/`room.*` governance rows (the trigger only maps `message.moderated`)」——**叶子已为显式写路铺好构建器**）；`MODERATION_OUTBOUND_ACTION` :150（truth-check 单源） |
| E8 | design doc `2026-08-07-aero-bus-b5-1-audit-outbox-status-machine.design.md` §5.3 | ⚠️→✅ **§5.2 才是写点清单**（§5.3 是 L1 聚合 [PROPOSED]）。§5.2：「message.create (extends `insert_outboxed`'s tx, message/idempotency.rs:158), message.edit, message.delete (extends `soft_delete_audited`'s tx, crud.rs:364), message.react, room.create, room.update, admin.<action> (extends the existing 20+ append_in_tx tokens) each write audit row + outbox row (status 0) in the same tx. **Rollback contract = E8 twin tests replicated per action class**」——本 direction 的问题陈述即 §5.2 的 DR2 落地 |
| E9 | （补充）aero-im-core 零审计引用 | ✅ **Verified**。`rg "AuditRepo|audit_governance|audit_events" crates/aero-im-core/src/` **零命中**（仅注释）——direction 断言成立 |
| E10 | （补充）`insert_outboxed` 生产调用点 | ✅ **Verified**。`MessageRepo::insert_outboxed`（message/idempotency.rs:158）生产调用者 = im-core `send_message_inner`（messages.rs:216）；其余调用点（event_outbox.rs:623/:794、message_side_effect.rs:442、notification_bundle.rs:552、events.rs:458）全部在 `#[cfg(test)]` 内。**send 的审计写可安全落在该 seam** |
| E11 | （补充）E8 回滚先例 | ✅ **Verified**。`audit_failure_rolls_back_delete_and_outbox_append`（message/events.rs db_tests）：`soft_delete_outboxed_system` 传不存在的 workspace → audit FK 违例 → 整事务回滚（消息保留、event_outbox 不增行）——**「审计写失败 = 域操作失败」的既有钉** |
| E12 | （补充）T-11 drill | ✅ **Verified**。`crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`：种子 N 行 status 0 → relay 对 closed token endpoint 跑 2 轮 → 断言 `COUNT(status=0)=N`、`COUNT(status IN (1,2,3))=0`、`SUM(attempts)=N→2N`、`last_error` 含 transport 失败（非空证明 relay 真跑过） |
| E13 | （补充）sibling spec 边界（**关键**） | ✅ **Verified**。`docs/requirements/2026-08-08-aero-common-b5-1-lane-mapping-in-tx-outbox.req.md` 已规定：5 个新 token（`LOCAL_ACTION_MESSAGE_CREATE/EDIT/DELETE` + `LOCAL_ACTION_ROOM_CREATE/ARCHIVE`）、`governance_lane_for` 扩 5 臂、`AuditGovernanceOutboxRepo::append_in_tx`（H3，落 `audit_governance.rs`）、5 条生产者 seam 接线（insert_outboxed / edit_outboxed_authorized / soft_delete_audited+soft_delete_outboxed_authorized / create_in_workspace_authorized / set_channel_archived_authorized）、**无条件入队**门语义（R6：不 consult runtime、无 binding RAISE；source_system 缺 binding 回退 `"aero-im"`）、同事务 fail-closed（R5）。**其词表/负例集与本 direction 有 1 处冲突**（`message.recalled` 被其列为 unmapped 负例，本 direction 验收要求 recall → outbox 行）——§1.1 更正③ + §6 协调 |

### 1.1 对 direction 陈述的勘误/钉化（evidence-backed）

- **更正① delete 已审计**：`delete_message` → `soft_delete_outboxed_authorized`（authorization.rs:458）在事务内解析 `access.workspace` 并以 `Some("message.deleted")` 走 `soft_delete_locked_outboxed_in_tx`（events.rs:333-344 条件 `append_in_tx`）——**audit_events 行已 in-tx 提交**。本 direction 对 delete 的增量 = **outbox 行**（审计行复用现有 AuditId）。
- **更正② recall 已审计**：`recall_locked_outboxed_in_tx`（authorization.rs:296-374）已 in-tx 写 `"message.recalled"` 审计行（actor=recaller、target=message id、detail={room_id,digest}），但**丢弃了 `append_in_tx` 返回的 AuditId**。增量 = 捕获 AuditId + outbox 行。
- **更正③ token 词表冲突（sibling 协调）**：sibling spec R3 把 `message.recalled` 列为 unmapped 负例（`unknown_local_token_passes_through_unmapped` 负例集 = `["call.join", "message.recalled", ""]`）；本 direction 验收 (a) **明确要求 recall 提交 outbox 行**。冲突以本 direction 验收为准（recall 的审计行已存在，映射是自然补全）：集成时 sibling 负例集 `message.recalled` 移除（负例由 `call.join`/`""` 承接），`governance_lane_for` 臂数 = 两腿并集（§6）。
- **方向性缺口钉化（全部 verified）**：零审计行 = `send_message`（insert_outboxed 无审计）、`edit_message`（`edit_locked_outboxed_in_tx` 无审计）、room.*/admin.* 治理操作（create_room_in_workspace / add_member / archive_channel / set_channel_meta / set_room_post_policy / slowmode / reaction-limit / retention / pin / unpin——storage seam 与 service 层均零审计引用，E2/E9）；零 outbox 行 = delete/recall 之外的全部上述操作（0239 trigger 只认 `message.moderated`，E5）。
- **「audit_governance.rs 存在」精确化**：`aero-storage/src/audit_governance.rs`（1123 行）目前**只含 DDL 契约 fixtures**，无 `AuditGovernanceOutboxRepo`（全仓零命中，H3 手记 :19-27 明言 sibling slice 落 repo）；sibling spec R4 已接管该交付。本 direction 消费之。

## 2. Verified current state（缺口盘点）

```
入队两条路（E5/E13）：
a) 触发器路   audit_events AFTER INSERT → aero_enqueue_governance_audit()
             └─ 仅 'message.moderated' → outbox（class 'admin'/priority 100）
                其余 token → RETURN NEW pass-through（fail-open 零 RAISE）
b) 显式写路   AuditGovernanceOutboxRepo（H3）—— sibling spec 交付，本 direction 消费

message.* 审计现状（E1/E3/E4/E11，全部 verified）：
  send      → 零 audit 行、零 outbox 行（insert_outboxed idempotency.rs:158）
  edit      → 零 audit 行、零 outbox 行（edit_locked_outboxed_in_tx events.rs）
  delete    → audit 行 in-tx（message.deleted，authorization.rs:458）；outbox 行 = 0
  recall    → audit 行 in-tx（message.recalled，authorization.rs:296-374）；outbox 行 = 0
  moderate  → audit 行 + outbox 行（trigger，E5）——唯一完整路径，不得双写

room.*/admin.* 审计现状（E2/E9）：
  create_room_in_workspace / add_member / archive_channel / set_channel_meta /
  set_room_post_policy / slowmode / reaction-limit / retention / pin / unpin
  → 全部零 audit 行、零 outbox 行

缺口（本 direction 关闭）：
  a) send/edit + 全部 room.* 治理操作无 audit_events 行（B5 relay 与审计轨迹不可见）
  b) delete/recall 有 audit 行但无 outbox 行（B5 relay 不可见）
  c) 无任何服务边界 oracle 证明「操作提交 ⇔ 审计行 + status-0 outbox 行同事务存在」
     （sibling spec 的 parity 测试走 storage seam；本 direction 补 svc.* 边界）
```

## 3. Scope

**In scope**：
- **本 direction 新增的 5 个本地 token**（在 sibling 5 token 之上，§4 R1）：`message.recalled`、`room.update`、`room.member.added`、`room.pin`、`room.unpin`——叶子常量 + `governance_lane_for` 臂 + truth-check 守卫按 sibling R1/R2/R3 同形扩展
- **本 direction 的 6 条生产者 seam 接线**（sibling R5 未覆盖的，§4 R2）：`recall_locked_outboxed_in_tx`（补 enqueue）、`patch_channel_authorized`、`set_channel_post_policy_authorized`、`set_channel_slowmode_authorized`、`set_channel_reaction_limit_authorized`、`set_channel_retention_authorized`（room.update）、`add_member_authorized`（room.member.added）、`pin_authorized`/`unpin_authorized`（room.pin/room.unpin）——均走 sibling R4 的 `AuditGovernanceOutboxRepo::append_in_tx`
- **服务边界保证**：direction 点名的全部 ImService 方法（send/edit/delete/recall/moderate_delete + create_room_in_workspace/add_member/archive_channel/set_channel_meta/set_room_post_policy/slowmode/reaction-limit/retention/pin/unpin）产生对应行；**零服务签名变更**
- **db_tests**（§4 R5-R8）：`crates/aero-im-core/src/db_tests/audit_governance_tests.rs`（relay_tests.rs 的 sibling）——验收 (a)-(d) 全部落地
- **既有钉位测试的受控更新**：sibling R7 枚举的 2 个换形 + 本 direction 的 sibling 负例集换形（§6）；`moderation_finalize_outbox_parity` 名称/字面量**不动**

**Out of scope**：
- `AuditGovernanceOutboxRepo`、叶子 5 token、`governance_lane_for` 5 臂、insert/edit/delete/room.create/room.archived 的 seam 接线、claim-order 测试 → **sibling spec 已接管**（本 direction 只消费 + 扩展剩余 token/臂）
- 0239/0240/0241 DDL 与 trigger、connector claim SQL、reconciler——**零改动**（触发路径仍是 `message.moderated` 唯一生产者，R-D2 保留）
- `message.react`（design doc §5.2 列出但 direction 验收未含）、`join_channel`/`leave_channel`（成员自助操作，direction 未点名）、`create_room`（legacy 无租户路径，落 nil workspace，direction 未点名）、unarchive（sibling R5 明示出范围）、系统编辑（unfurl/transcribe 的 `edit_outboxed_system`，仅授权路径审计）
- L1 聚合（[PROPOSED]，B5-3）；新 harness 腿 / 新 37-slot / 迁移——**零新增**
- auth sibling 的 SAVEPOINT fail-open——本车道走同事务 fail-closed（sibling R6/D1）

## 4. Requirements

### R1 — 叶子 token 词表补全（在 sibling R1 之上 +5）

`crates/aero-common/src/model/audit.rs` action-token 区新增 5 常量（值与既有生产字面量/映射测试候选串逐字一致）：

| 常量 | 值 | 车道 class |
|---|---|---|
| `LOCAL_ACTION_MESSAGE_RECALL` | `"message.recalled"` | message |
| `LOCAL_ACTION_ROOM_UPDATE` | `"room.update"` | room |
| `LOCAL_ACTION_ROOM_MEMBER_ADDED` | `"room.member.added"` | room |
| `LOCAL_ACTION_ROOM_PIN` | `"room.pin"` | room |
| `LOCAL_ACTION_ROOM_UNPIN` | `"room.unpin"` | room |

- 与 sibling R1 同区块、同 doc 纪律（「生产调用点一律经常量拼写，绝不裸字面量」）；truth-check token 单源守卫按 sibling R2 扩展到全 11 token（10 新 + `message.moderated`）。
- **`message.recalled` 是既有审计 token**（authorization.rs:368 生产字面量，E4/E11 先例）——常量化后生产点换常量；**不是新语义**。
- 不新增出站契约 token、不新增 envelope 类型（`AuditClaimPayload::new` 即构建器，E7）。

### R2 — `governance_lane_for` 补 5 臂（aero-ai，sibling R3 同形扩展）

- 新 5 臂：`message.recalled` → `{class: GOVERNANCE_CLASS_MESSAGE, priority: GOVERNANCE_PRIORITY_BACKLOG(10), outbound_action: 本地 token 自身, status: 0}`；`room.update`/`room.member.added`/`room.pin`/`room.unpin` → 同形但 `class: GOVERNANCE_CLASS_ROOM`。**admin 车道保持仅 `message.moderated`**（`is_admin_class` 对新 10 token 全 false）。
- sibling R3 换形联动：`unknown_local_token_passes_through_unmapped` 负例集从 `["call.join", "message.recalled", ""]` 改 `["call.join", ""]`；新增 per-token 映射 pin（每 token → class/priority=10/outbound=自身/status=0）。
- 本 direction 交付 = 5 臂 + 测试换形；sibling 的 5 臂由 sibling spec 交付。**集成后 11 臂全绿**（§6 手接）。

### R3 — 生产者 seam 接线（本 direction 的 6 条，sibling R5 未覆盖）

统一模式（镜像 sibling R5 逐字）：每条 seam 在其既有事务内追加「① 域变更 → ② `AuditRepo::append_in_tx`（返回 AuditId）→ ③ `AuditGovernanceOutboxRepo::append_in_tx(tx, audit_id, class, priority, payload)`」；**任一失败 → 整事务回滚（0 行逃逸）**。token 经叶子常量拼写（R1 守卫）。

| 本地 token | 生产者 seam（已 verified 调用点） | 接线要点 |
|---|---|---|
| `message.recalled` | `recall_locked_outboxed_in_tx`（authorization.rs:296） | audit 已 in-tx（:366-374），**捕获现有 `append_in_tx` 的返回值**（当前丢弃）补 outbox 行；已删除/缺失/重放路径（`Ok(None)`/已 recalled）不写；actor = recaller；target = message id；detail 沿用 {room_id, digest} |
| `room.update` | `patch_channel_authorized`（room/governance.rs:417）· `set_channel_post_policy_authorized`（:479）· `set_channel_slowmode_authorized`（:504）· `set_channel_reaction_limit_authorized`（:526）· `set_channel_retention_authorized`（:552） | 已 tx，`workspace` 已由 `lock_channel_aggregate` 解析；actor = caller；target = room id；detail = {room_id} + 便宜的 op 字段（policy/seconds/limit/days/archived 等） |
| `room.member.added` | `add_member_authorized`（room/governance.rs:173） | 已 tx，workspace 已解析；actor = caller；target = room id；detail = {room_id, member}；`ON CONFLICT DO NOTHING` 未插入（`Ok(false)`）不写 |
| `room.pin` / `room.unpin` | `PinRepo::pin_authorized` / `unpin_authorized`（pin.rs:31/:69） | 已 tx；workspace 经 `SELECT workspace_id FROM rooms WHERE id=$1` in-tx 解析；actor = actor；target = room id；detail = {room_id, message_id}；未创建/未移除（`false`）不写 |

**moderation 路径不得双写**（sibling R5 硬线，本 direction 继承）：`soft_delete_outboxed_system` 传 `message.moderated` 时跳过显式入队——trigger 是该 token 唯一生产者。`soft_delete_outboxed_system` 传 `message.deleted` 时的分支判据按 sibling R5（`governance_lane_for(audit_action)` + `audit_action != LOCAL_ACTION_MODERATED`）。

**sibling R5 已接管、本 direction 消费**：insert_outboxed（message.create）、edit_outboxed_authorized（message.edit，含「version 冲突路径不写 audit」）、soft_delete_outboxed_authorized（message.deleted 补 enqueue）、create_in_workspace_authorized（room.create）、set_channel_archived_authorized（room.archived，archived=true 才写）。

### R4 — 门语义与入队契约（消费 sibling R4/R6，本 direction 钉服务边界后果）

- **无条件入队**：app 路径不 consult `snaplink_commercial_runtime.enabled`、不做 binding RAISE——send/edit/delete/recall 与房间生命周期**不因商业开关或缺 binding 失败**（sibling R6；`source_system` 缺 binding 回退 `"aero-im"`）。
- **同事务 fail-closed**：audit/outbox 写失败 = 域操作失败（整事务回滚）——镜像 crud.rs:359-363「a message is never silently deleted unaudited」与 E11。
- 行契约（0239 CHECK 值域内）：`status=0`、`event_id` = `audit_events.id`（1:1）、`class`/`priority` 按 R1/R2 词表、payload = `AuditClaimPayload::new` 16-key 信封（`occurred_at` 经 `SELECT to_jsonb(created_at) #>> '{}'` 从 PG 取拼写；envelope `payload` 字段 = `aero_snaplink_audit_payload(detail)`，与 0239 trigger 同函数）、`ON CONFLICT (event_id) DO NOTHING`（幂等，重放/未来 trigger 双写无害）。
- **服务边界后果**（本 direction 的验收前提）：上述行在 `svc.send_message`/`edit_message`/`delete_message`/`recall_message`/`archive_channel`/`set_room_post_policy`/`pin_message` 等返回 Ok 时**必然已提交**；幂等重放（`send_message_idempotent` 命中 dedup、delete 已删、pin 已存在）不增行。

### R5 — 服务边界 oracle db_tests（验收 (a)，`db_tests/audit_governance_tests.rs`）

新文件（db_tests.rs 已 1014 行 > 800 WARN，新测试落 sibling 文件，同 relay_tests.rs 纪律）+ `db_tests.rs` mod 列表注册。每个 drill 双半部：

- **提交半部（服务边界）**：fixture = throwaway workspace（`WorkspaceRepo::create`）+ channel/group room（`rooms.create_in_workspace`，recall_tests 先例）+ owner/member；`service()` + `.with_pins(...)`（pin drill）。驱动 `svc.<op>` 后断言：`audit_events` 恰 1 行（`workspace_id` = fixture workspace、`action` = 词表 token、`actor_id` = 操作者、`target` = message id / room id）+ `audit_governance_outbox` 恰 1 行（`status=0`、`event_id` = 该 audit 行 id、class/priority 按 R2、payload 含 `action` = token 与 `idempotency_key` = event_id）。**drill 集**：send/edit/delete/recall/archive_channel/set_room_post_policy/pin（验收点名）+ 表驱动全词表扫（10 个 app token 各经其 svc 方法产 1 行——覆盖 create_room_in_workspace/add_member/set_channel_meta/slowmode/reaction-limit/retention/unpin）。
- **回滚半部（同事务，服务边界）**：置 `snaplink_commercial_runtime.enabled = TRUE` 且 fixture workspace **无 binding** → 同一 `svc.<op>` 返回 Err（v1 0236 `audit_events_snaplink_delivery` trigger 对 enforcement-on + 缺 binding 的 workspace RAISE `commercial binding is unavailable`，0236:68-84——**全操作类统一可注入失败**，因每条 seam 都写 audit_events）→ 断言 **域行 0 逃逸**（消息未插入/未删除/未召回/pin 不存在）+ `audit_events` 0 行 + `audit_governance_outbox` 0 行。测后 `restore_enforcement_disabled`（全局 singleton，audit_governance.rs:173-182 先例；harness 以 `--test-threads=1` 串行，纪律同 aero-storage）。
- 互补（非本文件）：audit 写本身失败的注入（毒 payload 违反 0239 CHECK / 不存在 workspace FK）由 sibling AC2 的 storage 级 drill 承担——本文件只做服务边界回滚证明。

### R6 — 车道映射 drill（验收 (b)）

- 表驱动：R5 提交半部产出的每行，断言 `class` == 叶子常量（`GOVERNANCE_CLASS_MESSAGE`/`GOVERNANCE_CLASS_ROOM`，im-core 可导入 aero-common）+ `priority` == 10（numeric，注释钉 `GOVERNANCE_PRIORITY_BACKLOG`——aero-storage 不得导入 aero-ai 的既有惯例，audit_governance.rs half-B :527-538 同款 comment-pin）；`message.moderated` 行（trigger 产，admin/100）不在本文件重复（`moderation_finalize_outbox_parity` 已钉）。
- **governance_lane_for 交叉钉链**（im-core 不依赖 aero-ai，Cargo.toml 只有 aero-common/aero-storage/aero-bus）：aero-ai 侧 per-token pin（sibling R3 + R2）使用与叶子相同的 `GOVERNANCE_CLASS_*` 常量 → 行值 == 叶子常量 == governance_lane_for 值，链式成立；priority 10/100 由 aero-storage db_tests（audit_governance.rs :566-567/:1071-1072）与迁移注释钉 aero-ai 常量。本 drill 的断言即该链的 DB 行端。
- **0239 DDL CHECK 探针**：直接 INSERT `class='bogus'` / `priority=0` / `delivery_mode='webhook'` / 非 object payload 各自报 CHECK 违例（证明产出行落在合法车道空间内）。

### R7 — fail-open pass-through 保持（验收 (c)）

- 0239 trigger **零改动**；drill：直接 `INSERT INTO audit_events`（不经生产者 seam，镜像 sibling R7 重写后的 `non_moderation_action_passes_through_unmapped` 直插形态）+ unmapped token（`'call.join'`）→ **无 RAISE、零 outbox 行**（trigger pass-through，fail-open）。
- app 词表闭合：`governance_lane_for(token) == None` 的 token 永不入队、永不 raise（R2 负例 + 本 drill 行端证明）。

### R8 — T-11 drill（验收 (d)）

- 经 `svc.*` 产 N 行（混合 message/room 类，≥2 token）→ **relay 缺席**（connector 是独立进程，db_tests 天然无 relay）→ 断言 `COUNT(status=0)=N`、`COUNT(status IN (1,2,3))=0`、`SUM(attempts)=0`、`last_error IS NULL`。
- **非空证明**（镜像 aero-audit-t11-drill.rs 语义）：以 connector `claim_due` 的 WHERE 谓词逐字镜像（`status IN (0,1) AND available_at <= clock_timestamp() AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())`，pg.rs:104-117，cross-pin 注释）查询 → 全部 N 行 due 可领（绝不静默假成功/假死）。

## 5. Testable acceptance mapping（direction acceptance 原句保留 + 可测试化）

| AC（原句） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **(a)** each of send/edit/delete/recall and archive_channel/set_room_post_policy/pin commits an `audit_events` row (action token, actor, target, workspace_id) AND an `audit_governance_outbox` row status=0 in the same transaction — forced rollback removes both (in-tx, same-tx assertion) | R5 提交半部：7 个点名 op 各经 `svc.*` 后 audit 行（workspace_id/action/actor/target 全字段断言）+ outbox 行（status=0、event_id=audit id 1:1）恰 1 行；表驱动全词表扫补 create_room_in_workspace/add_member/set_channel_meta/slowmode/reaction-limit/retention/unpin。R5 回滚半部：enforcement-on + 无 binding → 同 op Err → 域行/audit/outbox **0 行逃逸**。幂等重放不增行（R4） | R5 + db_tests/audit_governance_tests.rs |
| **(b)** action→lane mapping drill pins class/priority against aero-ai governance_lane_for and the 0239 DDL CHECKs | R6：每行 class == 叶子 `GOVERNANCE_CLASS_*`、priority == 10（comment-pin `GOVERNANCE_PRIORITY_BACKLOG`）——governance_lane_for 用同叶子常量（sibling R3），链式交叉钉；CHECK 探针（class='bogus'/priority=0/mode='webhook'/非 object payload 均违例） | R6 |
| **(c)** non-moderated actions pass through the trigger with no RAISE (fail-open preserved, 0239 R-D2) | R7：直插 `'call.join'` audit 行 → 无 RAISE、零 outbox 行；`governance_lane_for` 负例（`call.join`/`""` → None）不新增臂 | R7 |
| **(d)** T-11 drill: im-core-produced rows stay status 0, never silently delivered or falsely dead, when the relay is absent (mirrors aero-audit-connector/src/bin/aero-audit-t11-drill.rs) | R8：svc 产 N 行 → status=0/attempts=0/无 1/2/3/last_error NULL + claim 谓词镜像全部 due（非空） | R8 |

## 6. Coordination & hard rules（AGENTS §4）

- **sibling 边界（最重要的手接面）**：`AuditGovernanceOutboxRepo`（H3）、叶子 5 token、`governance_lane_for` 5 臂、insert/edit/delete/room.create/room.archived 接线 → sibling spec 已规定，本 direction **只补** 5 token + 5 臂 + 6 条 seam + 服务边界测试。两腿都动 `aero-common/src/model/audit.rs`、`aero-ai/src/governance.rs`、`aero-storage/src/{message,room/governance,pin}*.rs`——集成时**手接共享文件**（AGENTS §4.1：拉新文件 + 合并同区块；`governance_lane_for` 最终 11 臂、truth-check 守卫 11 token、audit_governance.rs 同模块 repo+fixtures 并存）。
- **sibling 负例集冲突（已决议）**：`message.recalled` 移出 `unknown_local_token_passes_through_unmapped` 负例集（负例由 `call.join`/`""` 承接）——本 direction 验收 (a) 优先；sibling AC5 的 `governance_lane_for("message.recalled") == None` 断言换为 mapped 断言。`user_delete_token_stays_out_of_admin_lane` 的 R-D2 不变量（`!is_admin_class("message.deleted")`）**以更强形式保留**（sibling R3 已规定）。
- **迁移纪律**：本 direction **零迁移**（0239/0240/0241 已落地，勿改）；trigger 仍是 `message.moderated` 唯一 SQL 生产者（R-D2）。
- **零 harness 改动**：新 db_tests 走既有 `cargo test --workspace --lib -- --ignored --test-threads=1` 主跑（harness `--skip db_tests::notifications_tests/relay_tests` 不涉及新模块）；`audit_governance::` 过滤腿是 `-p aero-storage` 域，不碰 im-core 测试名；37-slot 零新增。
- **全局状态纪律**：回滚半部翻转 `snaplink_commercial_runtime` 全局 singleton——测后必 restore（audit_governance.rs 先例），断言一律 workspace/event_id 作用域（共享非空库跑 ignored 套件）。
- **尺寸纪律**（`scripts/file-size-check.sh` 800 WARN / 1200 HARD）：新测试落 `db_tests/audit_governance_tests.rs`（db_tests.rs 1014 不再增长）；seam 接线只加 helper 调用（每点 ≤15 行）；`room/governance.rs`（现 801）与 `message/idempotency.rs`（现 1028）的增量不得推过 1200 HARD；`AuditGovernanceOutboxRepo` 落 sibling 指定模块，本 direction 不新建重复 repo。
- **字面量纪律**：新 Rust 代码不拼 token 字面量（truth-check 扩展后全仓扫描）；`"admin.content.flag"` 仍只许 audit.rs:150（AUDIT-FLAG mirror）。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· db_tests 以 `DATABASE_URL` + throwaway 已迁移库跑（AGENTS §4.3）。
- **活验证**：全新一次性库建库→migrate→跑新 db_tests→DROP；`make migrate-smoke` 不受影响（零迁移）。
