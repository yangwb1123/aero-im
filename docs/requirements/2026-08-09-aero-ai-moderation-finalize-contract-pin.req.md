# Requirements Spec — aero-ai：moderation finalize 元组的纯函数 outbox-contract pin（漂移守卫，可扩展到 admin.moderation.action）

- **Module (analysis root)**: `crates/aero-ai/src`（`governance.rs` = 契约映射面；交付面仅限 aero-ai 自身 + `lib.rs` re-export 链）
- **Direction**: "Pure outbox-contract pin for the moderation finalize tuple (drift guard, extensible to admin.moderation.action)"（value 6 / risk_reduction 6 / effort 2 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-f8cd3622.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（B5 contract item 3：`admin.content.flag` ↔ `admin.moderation.action` 择一锁定——已锁 `admin.content.flag`）
- **Sibling specs（同批次，文件面协调）**: `2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.req.md`、`2026-08-08-aero-im-core-moderate-delete-none-workspace-fail-closed.req.md`（R-D1 三生产者对称）、`2026-08-08-aero-eng-relay-runtime-health-leg.req.md`（T-11 钻取/relay 腿）。本 spec 不触碰 aero-storage/aero-im-core/aero-audit-connector 任何文件——只读参照
- **Status**: Requirements（下述证据全部经源码 grep 核对，2026-08-09；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-09

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-ai/src/governance.rs` — tests `admin_class_rows_never_aggregated` / `outbound_action_is_single_contract_token` / `user_delete_token_stays_out_of_admin_lane` | ✅ **Verified（全部命中）**。三测试均在 `#[cfg(test)] mod tests`（:97 起）内，**无 `#[ignore]`、无 DATABASE_URL**——plain `cargo test` 执行。`user_delete_token_stays_out_of_admin_lane` 断言 `governance_lane_for("message.deleted") == None` + `!is_admin_class("message.deleted")`（R-D2 负 pin）。另有 `moderation_lane_preempts_backlog_under_desc_claim`（100 > 10 DESC 优先）、`unknown_local_token_passes_through_unmapped`（含 "message.deleted"）、`mapping_is_token_keyed`、`outbound_action_is_single_contract_token`（outbound token = `MODERATION_OUTBOUND_ACTION`、class `admin`、status 0）共 6 测试 |
| E2 | `crates/aero-ai/src/worker/mod.rs` `handle_moderate`（`LOCAL_ACTION_MODERATED` + detail payload + `moderation_delete_workspace` R-D1） | ✅ **Verified**。`handle_moderate` :372（符号稳定）；BLOCK 分支 :388-408：`moderation_delete_workspace(job)?`（:389，R-D1，helper :183-190 拒绝 workspace_id=None）→ detail `{"reason": reason, "source": "ai_worker"}` → `soft_delete_outboxed_system(id, Some(workspace), None, Some(LOCAL_ACTION_MODERATED), detail, ParticipantId::nil(), None)`。**worker 只"隐含"元组——没有任何 aero-ai 函数返回它**（direction 问题陈述准确） |
| E3 | `crates/aero-storage/src/audit_governance.rs` — drill fixture to mirror | ✅ **Verified**。`moderate_finalize` :183-198 = 生产 seam 的精确镜像（模块 doc 明言 "The exact production seam `AiWorker::handle_moderate` calls"）；`moderation_finalize_outbox_parity`（`#[ignore = "requires live Postgres"]`，DB-gated）断言 half 2（audit 行：action `"message.moderated"` 原样、actor NULL、target = message id）+ half 3（governance 行：`event_id` 1:1、**status 0、class 'admin'、priority 100（字面量）**、`payload->>'action'` = `MODERATION_OUTBOUND_ACTION`）。`governance_rows_for` :216（workspace-scoped 计数）。**direction 的"只在 DB-gated drills 断言"准确** |
| E4 | `crates/aero-storage/src/message/events.rs:267` `soft_delete_outboxed_system` | ✅ **Verified（行号精确命中 :267）**。`Option<&str> audit_action` 参数；审计行仅在 `(workspace, action)` 均 Some 时同 tx append（:330-336）；deleted 事件 outbox 无条件追加。0239 触发器据此派生 outbox 行 |
| E5（支撑） | `crates/aero-common/src/model/audit.rs` 叶子 token | ✅ **Verified**。:150 `MODERATION_OUTBOUND_ACTION = "admin.content.flag"`；:156 `LOCAL_ACTION_MODERATED = "message.moderated"`；:143-147 注释明言 contract proposal 曾并列 `admin.content.flag` / `admin.moderation.action`，**锁定为 ONE constant**；:436 `vocabulary_consts_are_pinned` 钉字面量（plain cargo test，叶子层） |
| E6（支撑） | migrations/0239（outbox DDL + token-keyed 触发器） | ✅ **Verified**。0239 存在；`status INTEGER NOT NULL DEFAULT 0 CHECK (0,1,2,3)`、`class TEXT DEFAULT 'message' CHECK ('admin','message','room')`、`priority SMALLINT DEFAULT 10 CHECK (>0)`、payload `'action' = 'admin.content.flag'`（:123 字面量）；触发器仅 `NEW.action = 'message.moderated'` 入队（:83，token-keyed R2）。0236 = v1 路径（本 direction 不涉） |
| E7（支撑） | worker 测试为 in-memory fakes | ✅ **Verified**。`worker/tests.rs` `FakeQueue`/`FakeRow`（:346-400）+ `mk_job`（:324，in-memory `AiJob`，target_id/workspace_id 默认 None）；Moderate jobs 只测预算/成本/重试语义——**零 storage 元组断言** |

### 1.1 对 direction 陈述的钉化（evidence-backed）

- **钉化① 现有 `governance_lane_for` 已返回 5 元组中的 4 个字段**：`GovernanceLane { class, priority, outbound_action, status }`（governance.rs :43-61）。direction 的"no aero-ai-side function returns the exact row contract"对**完整 5 元组**（含 `audit_action` = 本地 token）成立——缺的恰是第 5 字段 + 一个按 `handle_moderate` 控制流（BLOCK verdict / target 存在）返回它的纯函数。新 fn **组合** `governance_lane_for`（唯一 match 臂留在原处，不复制 match）。
- **钉化② `(job, verdict)` 签名 = 元组镜像，非决策镜像**：`handle_moderate` 的 R-D1 workspace 门（`moderation_delete_workspace`，worker/mod.rs :183-190）决定 finalize **是否运行**，不改变元组本身——元组是 `LOCAL_ACTION_MODERATED` 的纯函数。故新 fn 不接 workspace 判定（避免复制 R-D1 逻辑、零 worker diff）；workspace 门保持原处，由既有 worker 单测（`moderation_delete_workspace_refuses_none_fail_closed`，worker/tests.rs）钉住。
- **钉化③ "explicitly rejected fail-closed" 的可测形态**：今日叶子只锁一个 outbound token；B5 item 3 场景（翻转为 `admin.moderation.action`）由叶子 `vocabulary_consts_are_pinned` + 本 spec 的 mirror 字面量测试在 **plain cargo test** 失败（fail-closed，无需 PG）。新增 admin.* token 场景由 `ADMIN_LANE_TOKENS` 词汇表 + 迭代测试钉住（§4 R3）。

## 2. Verified current state（缺口盘点）

```
5 元组契约链（全部 verified）：
  worker 隐含：  handle_moderate :396-408 → soft_delete_outboxed_system(..., Some(LOCAL_ACTION_MODERATED), ...)
  storage 派生： 0239 触发器（token-keyed，仅 'message.moderated' 入队）→ audit_governance_outbox 行
  DB-gated 断言： moderation_finalize_outbox_parity（audit_governance.rs，#[ignore]）half 2+3 全形状
  plain 断言：   governance.rs 现有契约测试 + `finalize_contract_for` 四个 plain 测试 —— 钉 token-keyed / R-D2 / DESC 优先 / outbound 单 token 与完整 5 元组

漂移检测面现状：
  token 漂移        → 叶子 vocabulary_consts_are_pinned（aero-common，plain）+ 0239 SQL 注释 pin（textual）
  映射漂移          → governance.rs 6 测试（plain）——但只到 4 元组
  完整 5 元组漂移    → ✅ `finalize_contract_mirrors_drill_fixture` plain 守卫
  admin.* 新 token  → ✅ `ADMIN_LANE_TOKENS` + map-or-reject plain 守卫（R-D2 负 pin）
```

**Gap this direction closes**（all verified）：① 一个纯 fn 返回 `handle_moderate` 最终提交的完整 5 元组；② 镜像 drill fixture 的 plain `cargo test` 测试（无 DATABASE_URL），任何 token/常量/DDL 字面量漂移在 CI 早段失败；③ admin.* 词汇表级 map-or-reject 守卫（R-D2 负 pin 保持）。

## 3. Scope

**In scope（effort 2 的完整切片）**：
- `crates/aero-ai/src/governance.rs`：新纯 fn `finalize_contract_for(job, verdict)` + 新 5 字段类型 `ModerationFinalizeContract` + 新生产常量 `ADMIN_LANE_TOKENS`（admin 类本地 token 词汇表，扩展点）。
- `crates/aero-ai/src/lib.rs`：governance re-export 链补三个新符号（既有 :37-39 链，追加即可）。
- `crates/aero-ai/src/governance.rs` `#[cfg(test)]`：新增 4 个 plain 单测（T-11 族命名，无 `#[ignore]`、无 DATABASE_URL）。
- 回归门：governance 既有 6 测试、worker 全部单测（含 R-D1 fail-closed 测试）保持绿；DB-gated drill `moderation_finalize_outbox_parity` 零改动、保持 PASS（harness 腿回归验证）。

**Out of scope**（direction 验收未点名，方向外）：
- `handle_moderate` 的任何生产改动（不接线——fn 是元组镜像，worker 已传 re-exported 常量 = 同源；接线会复制 R-D1 判定或改动控制流，超出 effort 2，见 §1.1 钉化②）。
- `governance_lane_for` 的语义改动（非 admin 未知 token → `None` pass-through 保持——R1 fail-open 设计，0239 需继续放行非 moderation 审计行）。
- `GovernanceLane` 结构改动（4 字段行元组保持原样，5 字段是新类型）。
- aero-storage / aero-im-core / aero-audit-connector / aero-common 的任何改动（叶子 pin 已存在，本 spec 只消费）。
- 0239 DDL / 触发器 / 迁移、claim ORDER BY / priority 值（B5-3 handoff）。
- 同分析文件的 direction #1（kind-aware budget reserve）与 #2（relay seam + 可观测）——另行 spec。

## 4. Requirements

### R1 — 纯函数 `governance::finalize_contract_for` 返回 handle_moderate 最终提交的完整 5 元组（本 direction 的核心增量）

`crates/aero-ai/src/governance.rs` 新增：

```rust
/// 完整 moderation finalize 行契约：audit_events 行 + audit_governance_outbox 行的
/// 5 字段并集，即 handle_moderate 传 LOCAL_ACTION_MODERATED 时 storage 0239 派生、
/// DB-gated drill（audit_governance.rs moderate_finalize）断言的元组。
pub struct ModerationFinalizeContract {
    pub audit_action: &'static str,    // LOCAL_ACTION_MODERATED（audit_events.action）
    pub class: &'static str,           // GOVERNANCE_CLASS_ADMIN（outbox.class）
    pub priority: i16,                 // GOVERNANCE_PRIORITY_MODERATION（outbox.priority，DESC）
    pub outbound_action: &'static str, // MODERATION_OUTBOUND_ACTION（outbox.payload.action）
    pub status: i16,                   // 0（outbox.status，enqueue-time 规范态）
}

/// 纯 + 总（对词汇表）：BLOCK verdict 且 target 存在 → Some(完整 5 元组)；否则 None。
/// 永不 raise、永不 panic、无 I/O。workspace 解析（R-D1）是调用方门（handle_moderate 的
/// moderation_delete_workspace），不在此处——元组与 workspace 无关。
#[must_use]
pub fn finalize_contract_for(job: &AiJob, verdict: Option<&str>) -> Option<ModerationFinalizeContract>
```

语义（镜像 `handle_moderate` :388-408 控制流）：
- `verdict.is_none()`（safe 判定）→ `None`（镜像 `if let Some(reason) = verdict` 分支）；
- `job.target_id.is_none()` → `None`（镜像 worker 的 no-target warn 分支）；
- 否则 → `Some(ModerationFinalizeContract { audit_action: LOCAL_ACTION_MODERATED, class: GOVERNANCE_CLASS_ADMIN, priority: GOVERNANCE_PRIORITY_MODERATION, outbound_action: MODERATION_OUTBOUND_ACTION, status: 0 })`——**组合** `governance_lane_for(LOCAL_ACTION_MODERATED)` 取后 4 字段（唯一 match 臂留在原处，不复制 match、不出现第二处字面量）。

**Acceptance**：
- A1.1 `finalize_contract_for` 存在于 governance.rs，纯（无 I/O、无 panic 路径、`#[must_use]`），签名 `(job: &AiJob, verdict: Option<&str>) -> Option<ModerationFinalizeContract>`；doc 注释明言它是 `handle_moderate` 提交的精确元组 + drill fixture 镜像。
- A1.2 `ModerationFinalizeContract` 恰含 5 字段（audit_action / class / priority / outbound_action / status），类型与 §4 一致；所有字段值来自 re-exported 叶子常量（`LOCAL_ACTION_MODERATED` / `GOVERNANCE_CLASS_ADMIN` / `GOVERNANCE_PRIORITY_MODERATION` / `MODERATION_OUTBOUND_ACTION`），生产代码零裸字面量。
- A1.3 三个新符号（`finalize_contract_for` / `ModerationFinalizeContract` / `ADMIN_LANE_TOKENS`）进入 `lib.rs` governance re-export 链（:37-39 追加），`aero_ai::governance::*` 与 `aero_ai::*` 路径均可 grep。

### R2 — plain `cargo test` mirror 套件（T-11 族）：镜像 DB-gated drill，任何漂移早段失败

在 governance.rs `#[cfg(test)]` 新增 4 个测试（无 `#[ignore]`、无 DATABASE_URL，`cargo test -p aero-ai` 即跑）。**镜像对象** = `moderation_finalize_outbox_parity` half 2+3 的断言值（audit_governance.rs :280-327）：`"message.moderated"` / `"admin"` / `100` / `MODERATION_OUTBOUND_ACTION`（=`"admin.content.flag"`）/ `0`。每个字段**双钉**：既断言等于 re-exported 常量（映射一致性），又断言等于 drill 字面量（leaf/DDL 漂移守卫）——常量漂移 → 字面量臂红；字面量/叶子翻转 → 常量臂红。测试 fixture 照 `worker/tests.rs` `mk_job` 模式（:324）构造 in-memory `AiJob`（`target_id: Some(...)`、workspace_id 可 None——见 A2.3）。

**Acceptance**：
- A2.1 `finalize_contract_mirrors_drill_fixture`：BLOCK verdict（`Some("spam")`）+ target 存在的 job → `Some(contract)`；逐字段断言 `(audit_action, class, priority, outbound_action, status)` = `("message.moderated", "admin", 100, "admin.content.flag", 0)`（字面量，与 drill 同值）**且** = `(LOCAL_ACTION_MODERATED, GOVERNANCE_CLASS_ADMIN, GOVERNANCE_PRIORITY_MODERATION, MODERATION_OUTBOUND_ACTION, 0)`（常量）。测试内注明双钉意图 + drill 镜像出处。
- A2.2 `finalize_contract_safe_verdict_yields_no_contract`：`verdict = None` → `None`；`finalize_contract_no_target_yields_no_contract`：target_id None → `None`（workspace_id None 也返回 `None`——元组与 workspace 无关，R-D1 门不在本 fn，注释明言）。
- A2.3 漂移矩阵（**每条都可执行验证**）：① `LOCAL_ACTION_MODERATED` 改值 → 叶子 `vocabulary_consts_are_pinned`（aero-common）红 + A2.1 字面量臂红；② `MODERATION_OUTBOUND_ACTION` 翻转 `admin.moderation.action`（B5 item 3 场景）→ 叶子 pin 红 + A2.1 字面量臂红；③ `GOVERNANCE_PRIORITY_MODERATION` 改值 → A2.1 字面量臂 + `moderation_lane_preempts_backlog_under_desc_claim` 红；④ status/class 改值 → A2.1 红（0239 CHECK 在 DB 层再兜）。全部在 `cargo test -p aero-ai` 失败——**不依赖 `--ignored` PG 套件**。
- A2.4 4 个新测试均在 `cargo test -p aero-ai`（plain，无 env）真实执行且绿——验收时运行确认，非 vacuous。

### R3 — admin.* token map-or-reject 守卫（词汇表级，fail-closed）+ R-D2 负 pin 保持

governance.rs 新增生产常量（映射 match 旁的扩展点）：

```rust
/// admin 类本地 audit token 的权威词汇表。扩展步骤（二选一必须全做，缺一测试红）：
/// 1) 新 admin.* token → 加进本表；2) 在 governance_lane_for 的 match 加映射臂。
/// 只加表不加臂 → A3.1 迭代测试红（fail-closed：静默 None pass-through 被拒）；
/// 只加臂不加表 → 词汇表不完整（映射存在但无记录，review 可见）。
pub const ADMIN_LANE_TOKENS: &[&str] = &[LOCAL_ACTION_MODERATED];
```

**Acceptance**：
- A3.1 `admin_lane_vocabulary_fully_mapped_or_fail_closed`：迭代 `ADMIN_LANE_TOKENS`，逐 token 断言 `governance_lane_for(t)` 为 `Some` 且 `(class, priority, status) == (GOVERNANCE_CLASS_ADMIN, GOVERNANCE_PRIORITY_MODERATION, 0)`；并对每个 token 经 `finalize_contract_for`（BLOCK verdict + target）断言完整 5 元组。向表加新 admin.* token 而忘加 match 臂 → 本测试红。
- A3.2 R-D2 负 pin 在词汇表层重断言：测试内 `assert!(!ADMIN_LANE_TOKENS.contains(&"message.deleted"))`；既有 `user_delete_token_stays_out_of_admin_lane` + `unknown_local_token_passes_through_unmapped`（断言 `"message.deleted"` → None + 非 admin-class）**零改动保持绿**。
- A3.3 B5 item 3 翻转场景（`MODERATION_OUTBOUND_ACTION` → `"admin.moderation.action"`）在 plain cargo test 失败（叶子 `vocabulary_consts_are_pinned` + A2.1 字面量臂）——fail-closed 路径无需 PG；0239 SQL 字面量 pin 由 DB drill 腿交叉兜底（回归门，零改动）。

### R4 — 既有测试全绿（回归门，零行为改动）

**Acceptance**：
- A4.1 governance 既有 6 测试（`moderation_lane_preempts_backlog_under_desc_claim` / `unknown_local_token_passes_through_unmapped` / `user_delete_token_stays_out_of_admin_lane` / `mapping_is_token_keyed` / `outbound_action_is_single_contract_token` / `admin_class_rows_never_aggregated`）断言零改动、保持绿。
- A4.2 worker 单测全绿，含 `moderation_delete_workspace_refuses_none_fail_closed`（R-D1，worker/tests.rs）——`finalize_contract_for` 不触碰该门（§1.1 钉化②），R-D1 语义原地不动。
- A4.3 DB-gated drill `moderation_finalize_outbox_parity`（audit_governance.rs）零改动；harness `--ignored` 腿回归 PASS（跨层交叉守卫，非本 direction 增量）。

## 5. Acceptance → testable mapping（逐条落点）

| Direction acceptance（原文） | Testable artifact | Location | Status |
|---|---|---|---|
| "New pure fn (e.g. governance::finalize_contract_for(job, verdict)) returning the exact (audit_action, class, priority, outbound_action, status) tuple handle_moderate implies" | R1：`finalize_contract_for(job: &AiJob, verdict: Option<&str>) -> Option<ModerationFinalizeContract>`（5 字段类型，组合 `governance_lane_for`） | crates/aero-ai/src/governance.rs + lib.rs re-export 链 | ✅ 已实现并通过单测 |
| "unit tests mirroring the storage drill fixture run in plain `cargo test` (no DATABASE_URL) and fail on any token/DDL divergence (T-11 group)" | R2：4 个 plain 单测（`finalize_contract_mirrors_drill_fixture` / `_safe_verdict_yields_no_contract` / `_no_target_yields_no_contract` / 漂移矩阵 A2.3 逐条可执行） | crates/aero-ai/src/governance.rs `#[cfg(test)]`；执行 = `cargo test -p aero-ai` | ✅ 已实现并通过 plain cargo test |
| "extending the match with a new admin.* token (admin.moderation.action) either maps to the admin lane or is explicitly rejected fail-closed, with the R-D2 negative re-asserted" | R3：`ADMIN_LANE_TOKENS` 词汇表 + `admin_lane_vocabulary_fully_mapped_or_fail_closed`（map-or-reject）+ R-D2 词汇表层重断言；翻转场景 = 叶子 pin + A2.1 字面量臂 fail-closed | crates/aero-ai/src/governance.rs（常量 + 测试） | ✅ 已实现并通过正/负词汇表测试 |
| "existing governance + worker tests stay green" | R4：governance 6 测试 + worker 全单测（含 R-D1）零改动绿；drill `moderation_finalize_outbox_parity` 零改动 PASS | 既有文件，回归门 | ✅ 既有（回归验证） |

## 6. Harness gates & coordination

- **执行腿**：新测试全部落在 plain `cargo test -p aero-ai`（lib 单测腿）——**不新增 b5-pin 37 槽**（37 槽 guard 只计 `--ignored` DB 腿的命名条目，`b5-pin.sh:37` `audit_governance::` / `:40` `t11-fail-closed` 槽零变化）。"T-11 族" = 语义归属（outbox/audit 契约测试族，命名 `finalize_contract_*`），非 harness 槽位。
- **交叉守卫分工**：aero-ai plain mirror（本 spec）→ token/常量/字面量漂移早段失败；aero-storage DB drill（`moderation_finalize_outbox_parity`，harness `--ignored` 腿，`DATABASE_URL` + throwaway 库）→ leaf↔DDL 跨层交叉，零改动保持 PASS（A4.3）。
- **Sibling 协调**：与 `2026-08-08-aero-im-core-b5-1-in-tx-audit-outbox-producer-seam.req.md`（direction #1，`AuditGovernanceOutboxRepo` + 宽 token 入队面）无重叠写面——本 spec 只写 `aero-ai/src/governance.rs` + `lib.rs`，storage 文件只读参照。R-D1 面由 `2026-08-08-aero-im-core-moderate-delete-none-workspace-fail-closed.req.md` 覆盖，本 spec 不重复。
- **提交前必过**（AGENTS.md §4.3）：`cargo check --workspace` · `cargo test --workspace --lib`（含新 4 测试）· `cargo clippy --workspace --all-targets` 零新警告 · `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规（`finalize_contract_for` 非 `with_*` builder、非孤儿模块，truth-check 不红；governance.rs 现 ~200 行，远低于 800 WARN 阈值，无文件拆分需求）。

## 7. Out of scope（明确不交付）

- `handle_moderate` / worker 任何生产改动（§1.1 钉化②：元组是 `LOCAL_ACTION_MODERATED` 的纯函数，worker 已传 re-exported 常量 = 同源；接线需复制 R-D1 判定或改控制流，超出 effort 2）。
- `governance_lane_for` / `GovernanceLane` / `is_admin_class` 的语义或签名改动（非 admin 未知 token 的 `None` pass-through 保持——R1 fail-open，0239 需继续放行非 moderation 行）。
- aero-storage / aero-im-core / aero-audit-connector / aero-common 的任何改动（叶子 pin 已存在；drill 零改动仅回归）。
- 0239/0236 DDL、触发器、迁移、claim ORDER BY / priority 值（B5-3 handoff，sibling 面）。
- 同分析 direction #1（budget reserve / kind-aware admission）与 #2（relay seam、admin-lane 可观测、B5-4 scope provisioning）——各自另行 spec。
- 新 outbox token 的实际引入（`admin.moderation.action` 的 mapping 臂）——本 spec 只钉**扩展机制**（map-or-reject），不实现新 token。
