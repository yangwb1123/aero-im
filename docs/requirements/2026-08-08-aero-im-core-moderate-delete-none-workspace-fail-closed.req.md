# Requirements Spec — aero-im-core：`moderate_delete` workspace=None 静默无审计分支关闭（moderation 优先级车道生产者侧完备性）

- **Module (analysis root)**: `crates/aero-im-core/src`（`ImService::moderate_delete` = moderation 生产者契约面）；交付面含 `crates/aero-storage`（`message_reports::review_authorized` 第三生产者 + `audit_governance.rs` 契约测试）、`crates/aero-ai`（worker R-D1 对照）、`crates/aero-server`（`moderation_bot` 调用面）
- **Direction**: "Close the workspace=None silent no-audit branch in ImService::moderate_delete (moderation-priority lane completeness from the producer side)"（value 6 / risk_reduction 8 / effort 3 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-im-core-src-e8fa8dff.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`（`docs/campaigns/campaign-aero-im-b5.yaml`）；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`
- **Sibling specs（同模块/同批次，文件面协调）**: `2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.req.md`（direction #1 的落地 spec——其 E1 已登记 `moderate_delete` 含 R-D1 拒绝、行号漂移 :619-627；本 spec 是 #3，重叠面 = `governance_drill_tests` / `message_reports.rs` 删除路径断言）、`2026-08-08-aero-im-core-b5-1-in-tx-audit-governance-enqueue.req.md`（同分析文件 direction #1 宽 token spec，未落地）
- **Status**: Requirements（下述证据全部经源码 grep 核对，2026-08-08；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

> ⚠️ **方向性事实更正（§1.1 逐条）**：direction 引用的 `messages.rs:632` `workspace.map(|_| "message.moderated")` **在 HEAD 已不存在**——commit `d8c732a`（"feat: complete production platform hardening"）已把它换成硬 R-D1 守卫（`workspace.ok_or_else(Error::Invalid)`），`moderate_delete(None)` 现在**拒绝删除**（Err 路径），生产缺陷已关闭。验收第一项（workspace=None 的 DB-gated 测试）也**已存在且强于验收下限**（R9 `drill_moderate_delete_none_workspace_refuses_like_rd1`，crash.rs）。本 direction 的**真实残余增量** = 验收第二项的第三生产者腿：`review_authorized`（message_reports.rs）的治理 outbox 行形状（status 0 / class 'admin' / priority 100 / `MODERATION_OUTBOUND_ACTION`）**无任何断言**——这是全仓唯一缺口（§4 R3）。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-im-core/src/service/messages.rs:632` — `workspace.map(\|_\| "message.moderated")`，None 分支 = 无审计 | ❌ **STALE（HEAD 已修复）**。`moderate_delete` :611-659；`workspace.map` 已不存在——:625-633 为硬守卫 `let workspace = workspace.ok_or_else(|| Error::Invalid("moderate_delete requires a workspace; refusing un-audited delete (R-D1)"))?`；:642-652 以 `Some(workspace)` + `Some("message.moderated")` 调 `soft_delete_outboxed_system`。`git log -S "moderate_delete requires a workspace"` → 引入于 `d8c732a`（HEAD 历史内）。参数类型仍为 `Option<WorkspaceId>`（守卫在运行时而非类型层，与 AiWorker 同构） |
| E2 | `crates/aero-ai/src/worker/mod.rs:383-388` — R-D1：无 workspace 拒绝删除（fail-closed 对照） | ✅ **Verified（行号微漂）**。`handle_moderate` :389 `let workspace = moderation_delete_workspace(job)?;`；helper :183-190：`job.workspace_id.map(...).ok_or_else(AiError::Invalid("moderation job {} has no workspace_id; refusing un-audited delete (R-D1)"))`——Err 传播 → retry → bounded DLQ。worker 单测 `moderation_delete_workspace_refuses_none_fail_closed`（worker/tests.rs:131-148）钉 None 拒绝 + Some 成功 |
| E3 | `crates/aero-server/src/moderation_bot.rs` — 以 job workspace 调 moderate_delete（happy path 掩盖 None 分支） | ✅ **Verified（实际是双重守卫）**。bot 在调用前**自己先挡一层**：`let Some(workspace) = workspace else { record_skip(SkipReason::WorkspaceUnresolvable, job.message_id); return; };`（:477-483，模块 doc :155 注明 R-D1 parity），再 `:483 .moderate_delete(job.message_id, Some(workspace), &reason, &digest)`——`SkipReason::WorkspaceUnresolvable` 变体 :155-158 文档明言 "must NOT be deleted un-audited"。调用面 = caller 守卫 + 服务层守卫双保险 |
| E4 | `crates/aero-storage/src/message_reports.rs` — `review_authorized`：第三 moderation 生产者，actor=Some(reviewer) | ✅ **Verified（类型层 fail-closed + 完整 parity 断言）**。`review_authorized` :240 签名 `workspace: WorkspaceId`（**非 Option——类型层杜绝 None**）；remove 分支 :300-308 `MessageRepo::soft_delete_locked_outboxed_in_tx(..., Some(workspace), Some(reviewer), Some("message.moderated"), ...)`（actor=reviewer、同 tx 审计 + 0239 触发 outbox）。db 测试 `remove_review_commits_decision_delete_audit_and_outbox_once` 现在同时断言 1 条 audit 行、1 条 deleted-event outbox，以及治理 outbox 的 status/class/priority/action/event_id 一致性。 |
| E5 | `crates/aero-common/src/model/audit.rs` — `MODERATION_OUTBOUND_ACTION` 叶子 token | ✅ **Verified**。:150 `pub const MODERATION_OUTBOUND_ACTION: &str = "admin.content.flag";`；:437 const 断言 `assert_eq!(MODERATION_OUTBOUND_ACTION, "admin.content.flag")`。0239 触发器（migrations/0239_audit_governance_outbox.sql:16,98,123）同值硬编码 |

### 1.1 对 direction 陈述的勘误/钉化（evidence-backed）

- **勘误① 生产缺陷已在 HEAD 关闭**：E1 的 `workspace.map` 分支被 `d8c732a` 的 R-D1 守卫取代（:625-633）。**本 direction 不需要任何生产代码改动**——`moderate_delete(None)` 现在返回 `Err(Invalid)`、消息保持可见、零 audit/零 governance/零 Deleted 广播（与 AiWorker `moderation_delete_workspace` 完全对称）。三生产者的 fail-closed 姿态现在是**同构的**：AiWorker = 运行时 Err（worker/mod.rs:183-190,389）；moderation_bot = caller 守卫 + 服务层守卫（moderation_bot.rs:477-483）；review_authorized = 类型层非 Option（message_reports.rs:240）。
- **勘误② 验收第一项已满足且更强**：direction 验收给「拒绝（Err，镜像 R-D1）**或**写 system-scoped audit 行」二选一——实现选了拒绝分支，且 R9 drill（`crates/aero-im-core/src/db_tests/governance_drill_tests/crash.rs:22-149`）断言**完整负包络**：`Err(Invalid)`、`deleted_at IS NULL`、blocks 原样、audit_events=0、audit_governance_outbox=0、event_outbox deleted 帧=0；控制半（`Some(ws)` + enforcement ON）断言 1 audit + 1 governance 行（status 0 / class 'admin' / priority 100）。R9 在 `--test-threads=1` 的 workspace 级 ignored 套件中执行（test-integration.sh:615-622 只 skip notifications/relay 两个 im-core 模块，governance_drill_tests 不 skip）。
- **勘误③ 验收第二项三腿现状已对齐**：AiWorker 腿 = `moderation_finalize_outbox_parity`（audit_governance.rs，经 `moderate_finalize` seam——模块 doc 明言 "The exact production seam `AiWorker::handle_moderate` calls"，断言 1 audit + 1 governance 行全形状含 payload action）；moderation_bot 腿 = R9 控制半 + 优先级/载荷契约钻取；`review_authorized` 腿 = `remove_review_commits_decision_delete_audit_and_outbox_once` 新增治理 outbox 完整形状与 event_id 1:1 断言。三腿均保持同一 status=0 / class=admin / priority=100 / action 契约。
- **行号漂移登记**：messages.rs:632（→:625-633 守卫区，符号 `moderate_delete` 稳定）；worker/mod.rs:383-388（→:389 调用点 + :183-190 helper）。**符号锚点全部命中**。

## 2. Verified current state（缺口盘点）

```
三生产者 fail-closed 姿态（全部 verified，HEAD 已对称）：
  AiWorker        handle_moderate → moderation_delete_workspace(job)?  → None → Err → retry → DLQ
  moderation_bot  caller 守卫（WorkspaceUnresolvable skip）+ moderate_delete 守卫 → None 永不达服务层
  review_authorized  workspace: WorkspaceId 非 Option → None 类型层不可表达
  └─ ImService::moderate_delete(None) → Err(Invalid)，消息保持可见，零副作用（R9 钉死）

验收面现状：
  a) None-workspace DB-gated 测试        → 已有且强于验收下限（R9，crash.rs:22-115，workspace 级 ignored 腿）
  b) 三生产者 parity                     → AiWorker 腿 ✅（moderation_finalize_outbox_parity，storage seam）
                                          moderation_bot 腿 ✅（R9 控制半 + R3/R4，真实 svc.moderate_delete）
                                          review_authorized 腿 ✅ 治理 outbox 行形状与 event_id 1:1 断言已补齐
  c) 钻取门                              → moderation-priority-drill（moderation-in-first-batch :276 /
                                          parity-501 :350）+ t11-fail-closed 均为 b5-pin.sh 37 槽执行槽
                                          （:40-41），test-integration.sh 命名条目驱动
```

**Gap this direction closed**（all verified）：验收第二项的 `review_authorized` 腿已补齐治理 outbox 行形状断言（status 0 / class 'admin' / priority 100 / payload.action = `MODERATION_OUTBOUND_ACTION` / event_id 1:1 with audit_events.id），与另外两条生产者腿形成统一 parity 断言集（§4 R3）。无生产代码、无迁移改动。

## 3. Scope

**In scope（effort 3 的完整切片）**：
- `crates/aero-storage/src/message_reports.rs` db_tests：`remove_review_commits_decision_delete_audit_and_outbox_once`（或同模块新增测试）补治理 outbox 断言——恰 1 行、status=0、class='admin'、priority=100、`payload->>'action'` = `MODERATION_OUTBOUND_ACTION`、`event_id` 1:1 于 audit_events.id、actor='reviewer' 语义（§4 R3）。
- 三生产者 parity 的**统一可执行形态**：以既有三测试（`moderation_finalize_outbox_parity` / R9 控制半 + R4 / review_authorized 补断言后）为 legs，验收标准 = 每腿断言同一契约元组；如落地为新增统一 drill（im-core `db_tests/governance_drill_tests/` 或 storage `audit_governance.rs`），命名须进 harness 槽位（§5/§6 协调）。
- 守卫回归门：R-D1 守卫（messages.rs:625-633）、R9 drill 负包络、三钻取门（`moderation-in-first-batch` / `parity-501` / `t11-fail-closed`）全部保持 PASS（§4 R1/R2/R4）。

**Out of scope**（验收未点名，方向外）：
- `moderate_delete` 的生产代码改动（None 分支已关闭，**零改动**）；签名类型化（`Option<WorkspaceId>` → `WorkspaceId`）——与 AiWorker 保持同构运行期守卫，不动。
- AiWorker / moderation_bot 两腿的重写（已有断言，只作 parity 参照）。
- 0239 触发器 / outbox DDL / relay 状态机 / `AuditGovernanceOutboxRepo`（direction #1 的 H3 切片，sibling spec 面）。
- `message_reports` 非 remove 分支（kept 决策）、报告审计之外的任何写路径 token 扩展。

## 4. Requirements

### R1 — `ImService::moderate_delete` 的 R-D1 守卫保持 fail-closed（生产，已落地，回归门）

`crates/aero-im-core/src/service/messages.rs` `moderate_delete` 必须以 `Error::Invalid` 拒绝 `workspace=None`，不得以任何路径提交软删。

**Acceptance**：
- A1.1 `moderate_delete(msg, None, reason, digest)` → `Err(Error::Invalid)`，错误串含 "R-D1"（messages.rs:625-633，符号 `moderate_delete` + `ok_or_else` 锚点）。**零改动**——本 requirement 是回归钉，不是新实现。
- A1.2 参数类型保持 `Option<WorkspaceId>`（运行期守卫，与 `aero-ai` worker `moderation_delete_workspace` :183-190 同构）。

### R2 — workspace=None 的 DB-gated 负包络测试保持（已落地，R9）

`crates/aero-im-core/src/db_tests/governance_drill_tests/crash.rs` `drill_moderate_delete_none_workspace_refuses_like_rd1`（R9，`#[ignore = "requires live Postgres (DATABASE_URL)"]`）保持断言：拒绝后消息仍 live（`deleted_at IS NULL` + blocks 原样）、`audit_events` 0 行、`audit_governance_outbox` 0 行、`event_outbox` deleted 帧 0 行；控制半（`Some(ws)` + enforcement ON）恰 1 audit + 1 governance 行（status 0 / class 'admin' / priority 100）。

**Acceptance**：
- A2.1 R9 测试存在且不 skip：`cargo test --workspace --lib --locked -- --ignored --test-threads=1`（test-integration.sh:615-622 的 workspace 腿，governance_drill_tests 不在 skip 名单）中 `drill_moderate_delete_none_workspace_refuses_like_rd1` 通过。
- A2.2 负包络五项断言（Err / live / 0 audit / 0 governance / 0 Deleted 帧）逐项在测试源码中可 grep（crash.rs:22-149，负包络区 :47-111、控制半 :113-147）。

### R3 — 三生产者 parity：每生产者恰一条 status=0 admin/priority-100 outbox 行（本 direction 的唯一增量）

每个 moderation 生产者（AiWorker / moderation_bot / `review_authorized`）的删除路径都必须产生**恰 1 条** `audit_events` 行（action `message.moderated`）+ **恰 1 条** `audit_governance_outbox` 行（status=0、class='admin'、priority=100、`payload->>'action'` = `MODERATION_OUTBOUND_ACTION`、`event_id` 1:1 于 audit_events.id）。AiWorker 腿与 moderation_bot 腿已有断言（§1.1 勘误③）；**review_authorized 腿缺失，为必补项**。

**Acceptance**：
- A3.1 `crates/aero-storage/src/message_reports.rs` db_tests 中，review_authorized remove 路径测试（`remove_review_commits_decision_delete_audit_and_outbox_once` :589 或同模块新增测试）补断言：`audit_governance_outbox` **恰 1 行**，且 `(status, class, priority)` = `(0, 'admin', 100)`、`payload->>'action'` = `MODERATION_OUTBOUND_ACTION`（"admin.content.flag"，common/src/model/audit.rs:150）、`payload->>'event_id'` = 对应 `audit_events.id::text`（join 或双查询比对）。audit 行断言（actor=reviewer、action `message.moderated`、target=message id）保持。
- A3.2 三腿契约元组一致：`moderation_finalize_outbox_parity`（audit_governance.rs:247，AiWorker seam）与 R4 `drill_payload_contract_16_key_envelope_via_moderate_delete`（governance_drill_tests.rs:578，bot 路径）的既有断言不改动、保持通过；如新增统一 parity 测试，其每条腿的断言集合 ⊆ 上述既有断言（不引入新契约面）。
- A3.3 三测试均在 `--ignored` DB-gated 套件内执行（`DATABASE_URL` + 已迁移），无 vacuous green（每条腿至少 1 条消息 + 1 条 outbox 行计数断言）。

### R4 — 钻取门保持 PASS（回归门，零改动）

moderation 优先级钻取与 T-11 fail-closed 钻取不因本 direction 的任何改动（含 R3 的新断言/新测试）改变行为。

**Acceptance**：
- A4.1 `moderation-priority-drill`（`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`）PASS 行保持：`drill: moderation-in-first-batch: PASS`（:276）与 `drill: parity-501: PASS`（:350）——admin/100 行仍在首批被 claim（优先级压 FIFO）。
- A4.2 `t11-fail-closed` drill（`aero-audit-t11-drill.rs`，test-integration.sh:354-368 驱动）保持 PASS：relay 缺席 ⇒ 行停留 status=0 pending、永不假 dead。
- A4.3 `scripts/b5-pin.sh` 37 槽 pin guard 绿：`moderation-priority-drill` / `t11-fail-closed` / `audit_governance::` / `moderation_finalize_outbox_parity` 四执行槽的 `B5-CHECK <name>: PASS` 行齐全（b5-pin.sh:38-41 槽表 + :73-80 verdict 协议）。

## 5. Acceptance → testable mapping（逐条落点）

| Acceptance | Testable artifact | Location | Status |
|---|---|---|---|
| A1.1/A1.2 | `moderate_delete` R-D1 守卫（符号 + 错误串） | crates/aero-im-core/src/service/messages.rs:625-633 | ✅ 已落地（d8c732a），回归钉 |
| A2.1/A2.2 | `drill_moderate_delete_none_workspace_refuses_like_rd1`（R9 负包络 + 控制半） | crates/aero-im-core/src/db_tests/governance_drill_tests/crash.rs:22-149 | ✅ 已落地，workspace 级 ignored 腿执行 |
| A3.1 | review_authorized remove 路径治理行形状断言（恰 1 行 + 四元组 + event_id 1:1） | crates/aero-storage/src/message_reports.rs db_tests | ✅ 已落地并通过 DB-gated parity 验证 |
| A3.2 | AiWorker 腿 `moderation_finalize_outbox_parity`（:247）· bot 腿 R4（:578）既有断言不动 | crates/aero-storage/src/audit_governance.rs · crates/aero-im-core/src/db_tests/governance_drill_tests.rs | ✅ 已落地，作为 parity 参照腿 |
| A3.3 | 三腿均 `--ignored` DB-gated + 计数断言（非 vacuous） | harness `--ignored --test-threads=1` 腿 + `audit_governance::` 命名槽 | ✅ 既有机制；A3.1 补丁落同机制 |
| A4.1 | `drill: moderation-in-first-batch: PASS` / `drill: parity-501: PASS` | crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs:276/:350 | ✅ 回归门 |
| A4.2 | `t11-fail-closed` drill（relay 缺席 ⇒ status 0 pending） | crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs + test-integration.sh:354-368 | ✅ 回归门 |
| A4.3 | b5-pin 37 槽 pin guard（4 执行槽 PASS 行） | scripts/b5-pin.sh:38-41 + :73-80 | ✅ 回归门 |

## 6. Harness gates & coordination

- **执行腿**：A2/A3 的 DB-gated 测试在 `scripts/test-integration.sh` 两条腿跑——(a) workspace 级 `cargo test --workspace --lib --locked -- --ignored --test-threads=1`（:615-622，governance_drill_tests / message_reports db_tests 均在此腿，只用 throwaway 库）；(b) `audit_governance::` 与 `moderation_finalize_outbox_parity` 命名槽（:318-325，各自 fresh throwaway DB，`run_migrated_integration` 空过滤守卫防 vacuous green）。A3.1 补丁落在 (a) 腿（message_reports db_tests），不新增命名槽即可满足；若落地为新增命名测试，须同步 b5-pin 槽表（37 槽 guard 只换名不加数，见 sibling 先例）。
- **钻取门**：A4 三门由 b5-pin.sh 槽表 + verdict 协议钉死（§4 R4）；R9/R3/R4 属 im-core `db_tests::` 不被 skip 名单排除（test-integration.sh:615-622 仅 skip notifications/relay 两模块）。
- **Sibling 协调**：direction #1 的 seam spec（`2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.req.md`）在 `audit_governance.rs` / `governance_drill_tests` 有更大改动面（AuditGovernanceOutboxRepo + 五 token 入队）——本 direction 的 R3 断言**只读**这些文件既有测试（moderate_finalize seam 与 R4 作为参照腿，不改动），唯一写点是 `message_reports.rs` db_tests（direction #1 的 token 集为 message.*/room.*，不含 report 删除路径）——无重叠写面。

## 7. Out of scope（明确不交付）

- `moderate_delete` / `soft_delete_outboxed_system` 的任何生产代码改动——None 分支已由 d8c732a 关闭，本 direction 零生产 diff。
- 迁移、DDL、触发器、relay/connector 改动（0239 不动）。
- `AuditGovernanceOutboxRepo`（handoff H3，direction #1 sibling 面）。
- review_authorized 之外的生产者 / 非 remove 分支 / 其他 audit token 的 lane 扩展。
- 三生产者统一 parity 测试的**新契约面**（A3.2 限定断言 ⊆ 既有断言，不发明新字段/新语义）。
