# Design — aero-ai：moderation finalize 5 元组的纯函数 outbox-contract pin（漂移守卫，map-or-reject 词汇表）

> Module: `crates/aero-ai`（`governance.rs` = 契约映射面；交付面仅 aero-ai 自身 + `lib.rs` re-export 链）。
> Requirements: `docs/requirements/2026-08-09-aero-ai-moderation-finalize-contract-pin.req.md`（R1-R4 / A1-A4）。
> Status: design, pre-implementation。零 DB 迁移、零 harness 槽位改动、零 worker 生产改动。

## 0. Evidence verification verdict

本 prompt 的证据视为**不可信主张**，全部逐条重新核对源码（2026-08-09）。**所有 9 项主张验证为真**；1 处需求文档措辞歧义（A2.2 括号语）在 §2.1 钉化解决。

| # | Claim（untrusted → verified） | 核对结果 |
|---|---|---|
| E1 | `governance.rs` 6 测试：`admin_class_rows_never_aggregated` / `outbound_action_is_single_contract_token` / `user_delete_token_stays_out_of_admin_lane` + `moderation_lane_preempts_backlog_under_desc_claim` / `unknown_local_token_passes_through_unmapped` / `mapping_is_token_keyed`，plain `#[cfg(test)]`，无 `#[ignore]`/DATABASE_URL | ✅ 全部命中（governance.rs:96-206 `mod tests`）；无一 `#[ignore]`。`user_delete_token_stays_out_of_admin_lane` 断言 `governance_lane_for("message.deleted")==None` + `!is_admin_class`（R-D2 负 pin） |
| E2 | `worker/mod.rs` `handle_moderate`（:372）+ BLOCK 分支 finalize 调用（`:396-408`）+ `moderation_delete_workspace`（R-D1，:183-190） | ✅ `handle_moderate` :372；`soft_delete_outboxed_system` 调用 :397-406（detail `{"reason","source":"ai_worker"}`，`Some(LOCAL_ACTION_MODERATED)`，`ParticipantId::nil()`）；R-D1 helper :183-190 拒 `workspace_id=None`。**worker 只隐含元组，无任何 aero-ai fn 返回它** —— 准确 |
| E3 | `audit_governance.rs` drill：`moderate_finalize` :183 = 精确 seam 镜像；`moderation_finalize_outbox_parity` :247 `#[ignore = "requires live Postgres"]` | ✅ `moderate_finalize` :183-198（doc 明言 "The exact production seam `AiWorker::handle_moderate` calls"，传字面量 `Some("message.moderated")`）；parity :246-247 `#[ignore]`，half 2（audit 行：action 原样 / actor NULL / target=message id）+ half 3（gov 行：event_id 1:1、status 0、class 'admin'、priority 100、payload `action` = `MODERATION_OUTBOUND_ACTION`） |
| E4 | `message/events.rs:267` `soft_delete_outboxed_system` | ✅ :267 精确命中；audit 行仅 `(workspace, action)` 均 Some 时同 tx append（:329-335）；deleted 事件 outbox 无条件 |
| E5 | 叶子 `audit.rs` token：:150 / :156 / :436 + :143-147 contract 注释 | ✅ `MODERATION_OUTBOUND_ACTION="admin.content.flag"` :150；`LOCAL_ACTION_MODERATED="message.moderated"` :156；`vocabulary_consts_are_pinned` :436（plain 测试）；:143-147 注释明言 `admin.content.flag` ↔ `admin.moderation.action` 锁为 ONE constant |
| E6 | 0239 DDL 元组：status 0 CHECK(0,1,2,3) / class 'admin' CHECK / priority 100 / payload action `admin.content.flag`（:123）/ 触发器仅 `message.moderated`（:83） | ✅ 全部命中（`migrations/0239_audit_governance_outbox.sql`） |
| E7 | worker 测试 in-memory fakes：`mk_job` :324 / `FakeQueue` :346 / `FakeRow` :353 | ✅ 全部命中；`mk_job` 产 in-memory `AiJob`（target_id/workspace_id 默认 None）；Moderate 测试零 storage 元组断言 |
| 钉化① | `governance_lane_for` 已返回 5 元组中 4 字段（缺 `audit_action`） | ✅ `GovernanceLane { class, priority, outbound_action, status }`（governance.rs:43-61）——缺的恰是第 5 字段 + 按 handle_moderate 控制流返回它的 fn |
| 钉化② | `(job, verdict)` = 元组镜像非决策镜像；R-D1 workspace 门留在 worker（零 worker diff） | ✅ `moderation_delete_workspace` 决定 finalize **是否运行**（:389），不改变元组；元组是 `LOCAL_ACTION_MODERATED` 的纯函数 |

补充核对（设计所需，非需求声称）：`AiJob` 定义在 `aero-storage/src/ai_job.rs:37`，root re-export `aero-storage/src/lib.rs:146`（`pub use ai_job::{AiJob, ...}`）；`aero-ai/Cargo.toml` 已依赖 aero-storage（:14）——governance.rs 增加 `use aero_storage::AiJob;` 不引入新 crate 依赖。基线 `cargo test -p aero-ai --lib` = **205 passed / 0 failed / 0 ignored**（已实测，绿基线成立）。`lib.rs` governance re-export 链在 :36-39。

## 1. API changes（唯一交付面）

### 1.1 `crates/aero-ai/src/governance.rs` — 新增 1 类型 + 1 纯 fn + 1 常量

```rust
// 文件头新增 import（aero-ai 已依赖 aero-storage，Cargo.toml 零改动）
use aero_storage::AiJob;

/// 完整 moderation finalize 行契约：audit_events 行（audit_action）+ 
/// audit_governance_outbox 行（class/priority/outbound_action/status）的 5 字段并集。
/// 即 handle_moderate（worker/mod.rs:397-406）传 LOCAL_ACTION_MODERATED 时 storage
/// 0239 触发器派生、DB-gated drill（audit_governance.rs `moderate_finalize`/parity）
/// 断言的元组。纯镜像——worker 不调用本类型（同源于 re-exported 常量，接线会复制
/// R-D1 判定，超出范围）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModerationFinalizeContract {
    pub audit_action: &'static str,    // LOCAL_ACTION_MODERATED（audit_events.action）
    pub class: &'static str,           // GOVERNANCE_CLASS_ADMIN（outbox.class）
    pub priority: i16,                 // GOVERNANCE_PRIORITY_MODERATION（outbox.priority，DESC）
    pub outbound_action: &'static str, // MODERATION_OUTBOUND_ACTION（outbox.payload.action）
    pub status: i16,                   // 0（outbox.status，enqueue-time 规范态）
}

/// 镜像 handle_moderate BLOCK 分支（:388-406）控制流的纯函数：
///   verdict.is_none()        → None（safe 判定，镜像 `if let Some(reason) = verdict`）
///   job.target_id.is_none()  → None（镜像 no-target warn 分支）
///   否则                     → Some(5 元组)，后 4 字段**组合** governance_lane_for
///                              （唯一 match 臂留在原处，不复制、无第二处字面量）。
/// workspace_id **永不参与判定**（R-D1 门是调用方 handle_moderate 的
/// moderation_delete_workspace，元组与 workspace 无关——见 §2.1）。
/// 永不 raise、永不 panic、无 I/O。
#[must_use]
pub fn finalize_contract_for(
    job: &AiJob,
    verdict: Option<&str>,
) -> Option<ModerationFinalizeContract> {
    if verdict.is_none() || job.target_id.is_none() {
        return None;
    }
    let lane = governance_lane_for(LOCAL_ACTION_MODERATED)?; // 臂被删 → None → A3.1 红
    Some(ModerationFinalizeContract {
        audit_action: LOCAL_ACTION_MODERATED,
        class: lane.class,
        priority: lane.priority,
        outbound_action: lane.outbound_action,
        status: lane.status,
    })
}

/// admin 类本地 audit token 的权威词汇表（map-or-reject 扩展点）。扩展协议：
/// 新 admin.* token 必须**同时**做两件事，缺一测试红 / review 可见：
///   1) 加入本表；2) 在 governance_lane_for 的 match 加映射臂。
/// 只加表不加臂 → admin_lane_vocabulary_fully_mapped_or_fail_closed 红（fail-closed，
/// 静默 None pass-through 被拒）；只加臂不加表 → 映射存在但词汇表无记录（review 可见）。
pub const ADMIN_LANE_TOKENS: &[&str] = &[LOCAL_ACTION_MODERATED];
```

类型选择说明：`class`/`outbound_action`/`audit_action` 用 `&'static str` 与 `GovernanceLane` 一致（叶子常量即 `&'static str`）；`priority`/`status` 用 `i16` 与 `GovernanceLane` 字段类型一致（PG 侧 sqlx 读 i32 是 DB 腿自己的映射，不在本类型职责内）。`Copy` 使测试逐字段断言与 `assert_eq!(a, b)` 无需 clone。

### 1.2 `crates/aero-ai/src/lib.rs` — re-export 链追加（:36-39）

```rust
pub use governance::{
    governance_lane_for, is_admin_class, GovernanceLane, GOVERNANCE_CLASS_ADMIN,
    GOVERNANCE_PRIORITY_MODERATION, MODERATION_OUTBOUND_ACTION,
    ADMIN_LANE_TOKENS, finalize_contract_for, ModerationFinalizeContract,
};
```

纯追加，零删除零改名 → `aero_ai::governance::*` 与 `aero_ai::*` 既有路径全部保持解析。

### 1.3 新测试（4 个 lib 测试 + 1 个 integration 编译 pin（§1.4），全 plain，无 `#[ignore]`/env）

governance.rs `#[cfg(test)] mod tests` 内追加（fixture 照 `worker/tests.rs` `mk_job` 模式内联构造 `AiJob`——字段全 pub 可直接建）：

| 测试 | 断言 | 对应 |
|---|---|---|
| `finalize_contract_mirrors_drill_fixture` | BLOCK verdict `Some("spam")` + target `Some(ulid)` + workspace `Some` → `Some(contract)`；**双钉**逐字段：字面量 `("message.moderated","admin",100,"admin.content.flag",0)`（drill 同值）+ 常量 `(LOCAL_ACTION_MODERATED, GOVERNANCE_CLASS_ADMIN, GOVERNANCE_PRIORITY_MODERATION, MODERATION_OUTBOUND_ACTION, 0)`；doc 注明：**字面量臂是 FM1-FM5 的主检测器**，常量臂近乎 tautological（只在 fn 硬编码字面量而非组合常量时咬合），属防硬编码冗余；drill 出处（audit_governance.rs parity half 2+3） | R2 A2.1 |
| `finalize_contract_safe_verdict_yields_no_contract` | `verdict=None`（target 存在）→ `None` | R2 A2.2 |
| `finalize_contract_no_target_yields_no_contract` | `target_id=None` → `None`；fixture 的 `workspace_id=None`（mk_job 默认）也断言 **workspace 独立**：同 fixture 改 target 为 `Some` → `Some(contract)`（workspace 仍 None）——注释明言 R-D1 门不在本 fn | R2 A2.2（歧义钉化见 §2.1） |
| `admin_lane_vocabulary_fully_mapped_or_fail_closed` | **首条正向成员断言** `assert!(ADMIN_LANE_TOKENS.contains(&LOCAL_ACTION_MODERATED))`——修复空词汇表 vacuity（词汇表清空后循环空转、负 pin 平凡通过，此断言红）；再迭代 `ADMIN_LANE_TOKENS`：`governance_lane_for(t)` 为 `Some` 且 `(class,priority,status)==(GOVERNANCE_CLASS_ADMIN, GOVERNANCE_PRIORITY_MODERATION, 0)`；每 token 经 `finalize_contract_for`（BLOCK + target）断言完整 5 元组；`assert!(!ADMIN_LANE_TOKENS.contains(&"message.deleted"))`（R-D2 词汇表层重断言）。**选 contains 而非 `assert_eq!(ADMIN_LANE_TOKENS, &[LOCAL_ACTION_MODERATED])`**：B5-3 落第二个 token 时 contains+循环零改动，assert_eq 需同步更新——扩展协议友好 | R3 A3.1/A3.2 |

### 1.4 `crates/aero-ai/tests/reexport_pin.rs`（新 integration 测试——re-export 链编译期 pin）

> `crates/aero-ai/tests/` 目录**新建**。integration 测试是独立 crate，从 **root 路径** `use aero_ai::...`——`aero_ai::governance::*` 模块路径在 re-export 被删后仍可解析，钉不住 A1.3；root 路径在任一符号掉链时编译红（E0432 unresolved import），先于任何测试运行。文件精确内容（照抄，4 空格缩进块）：

    //! Root re-export chain compile-time pin (A1.3).
    //!
    //! These three symbols must stay resolvable as `aero_ai::{...}` — the ROOT
    //! path, not just `aero_ai::governance::...` (the module path would resolve
    //! regardless of the lib.rs re-export and pin nothing). Dropping any symbol
    //! from the re-export chain breaks this file's compile (E0432 unresolved
    //! import) → `cargo test -p aero-ai` red before any test runs.

    use aero_ai::{finalize_contract_for, ModerationFinalizeContract, ADMIN_LANE_TOKENS};

    #[test]
    fn root_reexports_resolve_and_are_usable() {
        // Trivial-but-real use keeps the file non-vacuous (a 0-test integration
        // shell would pass silently); the load-bearing pin is the import above.
        assert!(!ADMIN_LANE_TOKENS.is_empty());
        let _ = std::any::type_name::<ModerationFinalizeContract>();
        // fn-pointer coercion also pins the signature (A1.1) at compile time.
        let _: fn(&aero_storage::AiJob, Option<&str>) -> Option<ModerationFinalizeContract> =
            finalize_contract_for;
    }

依赖说明：`aero_ai`（被测）与 `aero_storage`（`AiJob` 用于签名 coercion）都是本包 `[dependencies]` → integration 测试 crate 直接可用，**零新依赖**；文件在 `src/` 树外 → truth-check 孤儿模块扫描不覆盖（只扫 `crates/*/src/`）；plain（无 `#[ignore]`）→ 零 harness 槽位影响。掉链检测：`cargo test -p aero-ai`（lib+integration 一起编译）与 `cargo check --workspace` 均红。

## 2. Compatibility constraints

1. **`GovernanceLane` / `governance_lane_for` / `is_admin_class` 语义零改动**：4 字段行元组保持原样；未知 token → `None` pass-through 保持（R1 fail-open——0239 触发器须继续放行非 moderation 审计行）；既有 6 测试零改动。
2. **worker `handle_moderate` 零 diff**：本 fn 是镜像，不接线。worker 已传 re-exported 常量 = 同源；接线需复制 R-D1 判定或改控制流，明确出范围。`#[must_use]` + doc 注释明言防误用。
3. **aero-common / aero-storage / aero-im-core / aero-audit-connector 零改动**（只读参照）。叶子 pin（`vocabulary_consts_are_pinned`）已存在，本切片只消费。
4. **零 DDL / 零迁移 / 零 harness 槽位**：0239/0236 不动；37 槽 b5-pin guard 只计 `--ignored` DB 腿命名条目（`b5-pin.sh:37` `audit_governance::` / `:40` `t11-fail-closed`），本切片全在 plain lib 腿，槽位零变化。
5. **`aero-ai/Cargo.toml` 零改动**：aero-storage 已是依赖（worker/mod.rs:48 已用 `aero_storage::{AiJob, AiJobKind}`）；governance.rs 新 import 是既有边。
6. **新符号全 pub + re-export** → 无 dead-code 警告；命名非 `with_*` builder → `truth-check.sh` 不红；governance.rs 现 206 行 → 新增后约 ~290 行，远低于 800 WARN 阈值，无拆分需求。
7. **新文件 `crates/aero-ai/tests/reexport_pin.rs`**（§1.4）：`tests/` 目录新建；包自身依赖即可编译，零新依赖；不在 `src/` 树 → truth-check 不扫；plain integration → 零 harness 槽位。**提交范围随之扩为 3 文件**（§4 step 10）。

### 2.1 需求文档歧义钉化（A2.2 括号语）

需求 A2.2 原文括号「workspace_id None 也返回 None」与钉化②「元组与 workspace 无关、R-D1 门不在本 fn」**表面冲突**。裁决：**`finalize_contract_for` 永不 consult `workspace_id`**。依据（同文件内更高优先级约束）——§1.1 钉化②（避免复制 R-D1 逻辑、零 worker diff）、§4 R1 语义（None 路径仅 `verdict.is_none()` 与 `target_id.is_none()` 两条）、A2.2 同句「元组与 workspace 无关，R-D1 门不在本 fn，注释明言」。括号语正确读法 = fixture 构造说明（mk_job 默认 workspace_id None，no-target 用例自然带 None 且结果仍 None），非语义约束。本设计以 **`finalize_contract_no_target_yields_no_contract` 内追加 workspace-独立断言**（workspace None + target Some → Some(contract)）把裁决变成可执行测试——实现者若按字面误读（workspace None → None）该测试红。

## 3. Failure modes（含检测路径，全 plain CI 或既定 DB 腿）

| # | 漂移/故障 | 影响 | 检测（何处红） | 时机 |
|---|---|---|---|---|
| FM1 | `LOCAL_ACTION_MODERATED` 改值 | 叶子 token 漂移 | 叶子 `vocabulary_consts_are_pinned`（aero-common plain）+ A2.1 字面量臂（run-scope：叶子 pin 需 `-p aero-common` 或 `--workspace` 观察，见 §4 step 7） | plain CI 早段 |
| FM2 | `MODERATION_OUTBOUND_ACTION` 翻转 `admin.moderation.action`（B5 item 3 场景） | outbound contract 翻转 | 叶子 pin + A2.1 字面量臂（run-scope 同上）；DB drill 腿交叉兜底（`gov[0].4["action"]` vs 常量） | plain CI 早段 |
| FM3 | `GOVERNANCE_PRIORITY_MODERATION` 改值（**突变方向钉死：100→5 降到 BACKLOG=10 之下**；100→200 只红 A2.1 字面量臂，DESC 既有测试不红） | 车道优先级漂移 | A2.1 字面量臂 + `moderation_lane_preempts_backlog_under_desc_claim`（既有，`> BACKLOG` 在 5 时红）**双红** | plain CI 早段 |
| FM4 | `GOVERNANCE_CLASS_ADMIN`（叶子常量）/ status（`governance_lane_for` 内**裸字面量 `0`，非常量**）改值 | 分类/状态漂移 | class：叶子 pin + A2.1 `"admin"` 字面量；status：A2.1 字面量 `0` + 既有 `outbound_action_is_single_contract_token`（`lane.status==0`）。⚠️ 0239 `CHECK (status IN (0,1,2,3))` **接受 1，不兜 status=0**——真 DB 守卫是 drill `gov[0].1==0`（--ignored 腿） | plain CI 早段 |
| FM5 | `governance_lane_for` 映射臂被删/改 | 组合来源消失/偏移 | `finalize_contract_for` 经 `?` 返 None 或 4 字段偏移 → A2.1 + A3.1 红 | plain CI 早段 |
| FM6 | 新 admin.* token 只加词汇表不加 match 臂 | 静默 None pass-through（R-D2 类漏洞） | `admin_lane_vocabulary_fully_mapped_or_fail_closed` 迭代红（fail-closed） | plain CI 早段 |
| FM6b | `ADMIN_LANE_TOKENS` 被清空/删除 | 词汇表真空（map-or-reject 守卫失效面） | T4 **正向成员断言** `assert!(ADMIN_LANE_TOKENS.contains(&LOCAL_ACTION_MODERATED))` 红——修复前循环空转 vacuous green | plain CI 早段 |
| FM7 | 新 admin.* token 只加 match 臂不加词汇表 | 映射存在但无记录 | **无自动检测**（测试无法枚举缺失条目）——词汇表 const doc 的扩展协议 + review 流程兜底；文档明示此限制（诚实边界） | review |
| FM8 | R-D1 workspace 门被削弱 | 无审计删除 | 既有 `moderation_delete_workspace_refuses_none_fail_closed`（worker/tests.rs:131，零改动）——§4 矩阵中为**文档化可选行**（零 worker diff） | plain CI |
| FM9 | 0239 DDL 字面量漂移（SQL 侧 status/class/priority/action） | leaf↔DDL 跨层漂移 | DB-gated drill `moderation_finalize_outbox_parity`（`--ignored` 腿，throwaway 库）——**迟到但既定**；mirror 测试钉的是 Rust 侧契约意图，不冒充 SQL 守卫 | harness DB 腿 |
| FM10 | 误接线（生产调 `finalize_contract_for` 取代软删路径） | 复制 R-D1 判定 / 行为漂移 | doc 注释明言镜像性质 + `#[must_use]`；review | review |

fail-open / fail-closed 边界：未知 token pass-through（R1）与 admin.* 词汇表 map-or-reject（R3）**不冲突**——前者是 `governance_lane_for` 对非 moderation 行的既有 fail-open（0239 须放行）；后者是 `ADMIN_LANE_TOKENS` 对 **admin 类** token 的 fail-closed 完整性守卫（只加表不加臂 = 静默丢失被拒）。两层正交。

## 4. Migration steps（零 DB 迁移；代码落地顺序）

> 本切片无迁移文件、无 env、无配置、无 harness 槽位。以下为**实施顺序**（含 AGENTS.md §4.1/§4.3 硬规则）。

1. **改 `crates/aero-ai/src/governance.rs`**：加 `use aero_storage::AiJob;` → 加 `ModerationFinalizeContract` → 加 `finalize_contract_for` → 加 `ADMIN_LANE_TOKENS`（§1.1 精确形态，字段值全部来自 re-exported 叶子常量，生产代码零裸字面量）。
2. **改 `crates/aero-ai/src/lib.rs`**：:36-39 re-export 链追加 3 符号（§1.2）。
3. **governance.rs `#[cfg(test)]` 加 4 测试**（§1.3：T4 含**正向成员断言**；T1 doc 注明字面量臂为主检测器）。
4. **新建 `crates/aero-ai/tests/reexport_pin.rs`**（§1.4 精确内容；`tests/` 目录新建）——re-export 链编译期 pin。
5. **`cargo build`**（工作区编译；本切片不涉迁移，但按 §4.1 惯例 build 先行）。
6. **plain 验证**：`cargo test -p aero-ai --lib` —— 205 既有 + 4 新增全绿；**`cargo test -p aero-ai`**（lib + `tests/reexport_pin.rs` 编译并跑 1 测试——`--workspace --lib` 不覆盖 integration 目标，必须显式跑）；`cargo test --workspace --lib` 全绿。
7. **漂移矩阵实测（A2.3 逐条执行，非口头承诺）**：下表**必测行**（FM1-FM6 + FM6b + re-export drop）每行——按行 run-scope 临时突变 → 观察命令红 → 恢复。FM8 / FM9-DB 为**文档化可选行**（零 worker diff / 需 rebuild+migrate；跑不跑都须知悉检测器存在）。

    | 行 | 临时突变 | run-scope（观察命令） | 预期红（检测器） |
    |---|---|---|---|
    | FM1 | 叶子 `LOCAL_ACTION_MODERATED` 翻值（audit.rs:156） | `cargo test -p aero-common --lib vocabulary_consts_are_pinned` **+** `cargo test -p aero-ai --lib finalize_contract_mirrors_drill_fixture`（或一次 `cargo test --workspace --lib`） | 叶子 pin 红（**aero-common 内**——`-p aero-ai` 单独跑观察不到）+ A2.1 字面量臂红 |
    | FM2 | 叶子 `MODERATION_OUTBOUND_ACTION` 翻为 `admin.moderation.action`（audit.rs:150） | 同上（`-p aero-common` + `-p aero-ai` 或 workspace） | 叶子 pin + A2.1 `"admin.content.flag"` 字面量 |
    | FM3 | `GOVERNANCE_PRIORITY_MODERATION` **100→5**（governance.rs:32，aero-ai 自身常量）。**方向钉死**：降到 BACKLOG=10 之下——100→200 只红 A2.1（DESC 测试不红），矩阵不用 | `cargo test -p aero-ai --lib` | A2.1 字面量 100 + 既有 `moderation_lane_preempts_backlog_under_desc_claim`（`> BACKLOG` 失败）**双红** |
    | FM4 | (a) 叶子 `GOVERNANCE_CLASS_ADMIN` 翻值；(b) `governance_lane_for` 内 **status 裸字面量 `0`→`1`**（status 不是常量——「改常量值」修正为「改值（常量或 status 字面量）」） | (a) `-p aero-common` + `-p aero-ai`；(b) `-p aero-ai` | (a) 叶子 pin + A2.1 `"admin"` 字面量；(b) A2.1 字面量 0 + 既有 `outbound_action_is_single_contract_token`（`lane.status==0`）。0239 `CHECK (status IN (0,1,2,3))` **接受 1，不兜 status=0**——真 DB 守卫是 drill `gov[0].1==0`（--ignored 腿） |
    | FM5（**必测**，原「可加测」提升） | 删 `governance_lane_for` 的 `LOCAL_ACTION_MODERATED` 臂（`_ => None` 兜底保持编译；单行突变，全套最廉价） | `cargo test -p aero-ai --lib` | T1 `.expect` panic + T4 迭代红（`?` 传播 None）——删除/偏移两方向都红 |
    | FM6 | `ADMIN_LANE_TOKENS` 追加 `"admin.moderation.action"`（不加 match 臂） | `cargo test -p aero-ai --lib admin_lane_vocabulary` | T4 迭代红（新 token → `governance_lane_for` None → fail-closed 断言失败） |
    | FM6b（新行） | `ADMIN_LANE_TOKENS` 清空 `&[]`（或整个删除） | `cargo test -p aero-ai --lib admin_lane_vocabulary` | T4 **正向成员断言**红（`contains(&LOCAL_ACTION_MODERATED)`）——修复前该突变 vacuous green（循环空转 + 负 pin 平凡通过） |
    | re-export drop（新行） | lib.rs re-export 链删 3 符号任一 | `cargo test -p aero-ai`（编译含 `tests/reexport_pin.rs`；`cargo check --workspace` 同红） | E0432 unresolved import——**编译期红，先于任何测试** |
    | FM8（可选行） | 削弱 `moderation_delete_workspace`（worker/mod.rs:183-190）接受 workspace=None | `cargo test -p aero-ai --lib moderation_delete_workspace_refuses_none_fail_closed` | 既有 worker/tests.rs:131 红 |
    | FM9-DB（可选行） | 0239 字面量翻转（如 class `'admin'`→`'adminx'`，:105） | `cargo build` → throwaway 库 migrate → `DATABASE_URL=… cargo test -p aero-storage --lib -- --ignored audit_governance` | drill `gov[0].2=="admin"` 红（in-tx oracle + count asserts，非 vacuous） |

8. **静态门**：`cargo clippy --workspace --all-targets`（含 `tests/reexport_pin.rs`）零新警告；`scripts/{truth-check,file-size-check,web-check}.sh` 0 违规（`tests/` 不在 truth-check 的 `crates/*/src/` 扫描树内）。
9. **DB 腿回归（A4.3）**：throwaway 库（`CREATE DATABASE` → 迁移 → 测试 → `DROP DATABASE`，勿动共享 dev 库）；`DATABASE_URL=... cargo test -p aero-storage --lib -- --ignored audit_governance` —— `moderation_finalize_outbox_parity` 零改动 PASS（跨层交叉守卫）。
10. **提交**：单 commit 限 `crates/aero-ai/src/{governance.rs,lib.rs}` + `crates/aero-ai/tests/reexport_pin.rs` **三文件**；commit message 引 req 文件 + 钉化裁决（§2.1）。

## 5. Testable acceptance mapping（逐条落点）

| Requirement acceptance | Testable artifact | 执行命令 | 判定 |
|---|---|---|---|
| R1 A1.1 纯 fn 存在、签名 `(job: &AiJob, verdict: Option<&str>) -> Option<ModerationFinalizeContract>`、`#[must_use]`、doc 明言镜像 | governance.rs `finalize_contract_for` 定义 + doc 注释（§1.1 形态） | `cargo test -p aero-ai --lib`（编译即证）+ grep `#[must_use]` | 编译绿 + 符号存在 |
| R1 A1.2 5 字段类型、值全来自叶子常量、生产零裸字面量 | `ModerationFinalizeContract` 定义；实现体仅出现 `LOCAL_ACTION_MODERATED` + `lane.*` 组合 | `grep -n 'finalize_contract_for' crates/aero-ai/src/governance.rs`（人工审实现体无字面量） | 审阅通过 |
| R1 A1.3 3 符号进 re-export 链，双路径可 grep + **编译期 pin** | lib.rs :36-39 追加 + `tests/reexport_pin.rs`（§1.4） | `rg 'finalize_contract_for\|ModerationFinalizeContract\|ADMIN_LANE_TOKENS' crates/aero-ai/src/lib.rs`（3 命中）+ `cargo test -p aero-ai`（tests/ 编译绿） | 3 命中 + 编译绿（掉链 → E0432 红） |
| R2 A2.1 drill 镜像双钉 | `finalize_contract_mirrors_drill_fixture` | `cargo test -p aero-ai --lib finalize_contract_mirrors_drill_fixture` | PASS |
| R2 A2.2 两 None 路径 + workspace 独立 | `finalize_contract_safe_verdict_yields_no_contract` / `finalize_contract_no_target_yields_no_contract`（含 workspace 独立断言） | 同上两测试 | PASS |
| R2 A2.3 漂移矩阵可执行 | §4 step 7 逐行突变流程（**必测 FM1-FM6 + FM6b + re-export drop**，每行带 run-scope；可选 FM8 / FM9-DB） | 每行：按行 run-scope 突变 → 观察命令红 → 恢复 | 每行观察到预期红 |
| R2 A2.4 非 vacuous | 4 lib 测试 + integration pin 在 plain 跑真实执行 | `cargo test -p aero-ai --lib 2>&1 \| grep finalize_contract` + `cargo test -p aero-ai --test reexport_pin` | 4 条 ok + 1 条 ok（pin 文件非 0-test shell） |
| R3 A3.1 map-or-reject | `admin_lane_vocabulary_fully_mapped_or_fail_closed`（含正向成员断言） | `cargo test -p aero-ai --lib admin_lane_vocabulary` | PASS；加表忘臂 → 红（FM6 突变）；清空词汇表 → 红（FM6b 突变） |
| R3 A3.2 R-D2 负 pin 重断言 + 既有测试零改动 | 测试内 `!ADMIN_LANE_TOKENS.contains(&"message.deleted")`；既有 `user_delete_token_stays_out_of_admin_lane` / `unknown_local_token_passes_through_unmapped` 原样 | `cargo test -p aero-ai --lib user_delete_token_stays_out_of_admin_lane` | PASS |
| R3 A3.3 B5 item 3 翻转 fail-closed | FM2 突变流程（叶子 pin + A2.1 字面量臂红，无 PG） | §4 step 7 FM2 行 | 红在 plain |
| R4 A4.1 governance 既有 6 测试绿 | 零改动断言 | `cargo test -p aero-ai --lib`（含 6 既有） | 全绿 |
| R4 A4.2 worker 全单测绿（含 R-D1） | 零改动 | `cargo test -p aero-ai --lib worker::` | 全绿 |
| R4 A4.3 drill 零改动 PASS | `moderation_finalize_outbox_parity` 原样 | throwaway 库 + `cargo test -p aero-storage --lib -- --ignored audit_governance` | PASS |

## 6. Out of scope（明确不交付）

- worker / `handle_moderate` 任何生产改动（钉化②：元组是 `LOCAL_ACTION_MODERATED` 的纯函数，接线需复制 R-D1 判定或改控制流）。
- `governance_lane_for` / `GovernanceLane` / `is_admin_class` 语义或签名改动（未知 token `None` pass-through 保持）。
- aero-common / aero-storage / aero-im-core / aero-audit-connector 任何改动（叶子 pin 既有；drill 零改动仅回归）。
- 0239/0236/0240/0241 DDL、触发器、claim ORDER BY / priority 值（B5-3 handoff）。
- 新 outbound token `admin.moderation.action` 的实际引入（本切片只钉 map-or-reject **扩展机制**，不实现新 token）。
- 同分析 direction #1（kind-aware budget reserve）、#2（relay seam + 可观测）——另行 spec。

## 7. 提交前必过清单（AGENTS.md §4.3）

`cargo check --workspace`（干净）· `cargo test --workspace --lib`（全绿，含 4 新测试）· **`cargo test -p aero-ai`（lib + `tests/reexport_pin.rs`——`--workspace --lib` 不覆盖 integration 目标，须显式跑）** · `cargo clippy --workspace --all-targets`（零新警告，含 pin 文件）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· DB 腿 `--ignored` 回归（throwaway 库，§4 step 9）。
