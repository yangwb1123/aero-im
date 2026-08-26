# Requirements Spec — aero-eng：relay runtime-health 腿（Q7 最老 claimed 行年龄 / stuck-lease fail-closed + `--relay` 统一 B5-2 mock-sink probe 接线，落家 = `crates/aero-eng`）

- **Module (analysis root)**: `crates/aero-eng` — `audit_provision.rs`（B5-4 psql-backed fail-closed 供给门 + B5-3 `--priority` drill 面）+ aero-cli `AuditProvisionCheck_` 臂接线
- **Direction**: "Add relay runtime-health leg to the gate: oldest-claimed age (stuck leases) + unified --relay mock-sink probe for B5-2 state machine (422→dead, claim validation, client_credentials/scope)"（value 7 / risk_reduction 7 / effort 6 / confidence 7）
- **Source analysis**: `docs/auto/analyses/crates-aero-eng-36facc3d.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`；gate anchor `docs/campaigns/implementation-gate.md`（G6 当前为 "48/48（27 个可执行 slot + 21 个 [PROPOSED]）、T-11、moderation 优先级"）
- **Sibling specs（同批次，命令面协调）**: `2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（命令本体 = 本 direction 的扩展宿主）、`2026-08-08-aero-eng-b5-1-operation-class-coverage-leg.req.md`（同模块 Q6 腿，先于本 direction 落地；其 R8 兄弟模块拆分纪律为本 direction 复用）、`2026-08-08-aero-cli-b5-2-relay-probe-bin-landing.req.md`（**probe bin 的交付 direction**——本 direction 只消费、不交付）、`2026-08-07-aero-cli-b5-2-relay-probe.design.md` + `2026-08-08-aero-cli-b5-2-relay-probe-bin-landing.design.md`（probe 契约源）
- **Status**: Implemented（历史证据核对日期 2026-08-08；Q7、`--relay`、probe 与 harness 已于 2026-08-20 复核）
- **当前状态更新（2026-08-20）**：Q7 claimed-age/stuck-lease、`audit-provision-check --relay`、9 场景 probe 及 integration legs E/F/F′ 均已落地并通过。下方 E6、§1.1、§2 与协调段落中关于“probe 缺失 / SKIP”的文字是历史核对记录；当前 file-gate 分支实际执行为 PASS。
- **Current B5 pin (2026-08-26)**：live `B5_CONTRACT_TEST_LIST` 为 48 个 slot（27 个非 `[PROPOSED]` 可执行 slot + 21 个 `[PROPOSED]` 仓外占位）；“executed”是 manifest 分类，不是本文件声称已经运行的测试数。
- **行号纪律**: 行号是核对时锚点、会漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs` Q4_SQL（oldest *pending* only）+ verdict()（无 claimed/lease 输入） | ✅ 全中。`Q4_SQL` = `SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 0`——**只查 status=0**；`G0239Counts` 仅有 `oldest_pending_secs: Option<i64>`，全文件零 `status = 1` 年龄查询。`verdict()` 三支（dead-first → relay 关+undelivered → consistent/healthy）**无任何 lease/claimed-age 输入**；`undelivered = v1.pending + v1.claimed + g0239.pending + g0239.claimed`。缺口本体确认 |
| E2 | `crates/aero-audit-connector/src/relay.rs:36-70`（MAX_LEASE_SECONDS=86400, MAX_BACKOFF_SECONDS=300, is_dead_at, clamped_lease, audit_backoff） | ✅ 符号全中（行号漂移无碍）：`pub const MAX_LEASE_SECONDS: i64 = 86_400`、`pub const MAX_BACKOFF_SECONDS: i64 = 300`、`pub const PERMANENT_DEAD_AT: i64 = 2`、`pub fn is_dead_at(attempts) -> bool`（≥2 即 dead）、`pub fn clamped_lease(Duration) -> Duration`（clamp [1, 86400]）、`pub fn audit_backoff(attempts) -> Duration`（`2^(attempts-1)` cap 300）。**aero-eng 不得 import 本文件**（E9 依赖审计）——86400 以 clone 字面量 + 交叉引用注释落家（本仓既有先例：relay.rs 自身即 "clone of `AiUsageRepo::MAX_LEASE_SECONDS`"） |
| E3 | `crates/aero-cli/src/main.rs:427-470`（'relay-probe' 臂；直接 spawn；exit-code passthrough；AERO_RELAY_PROBE_BIN） | ✅ 全中（`Network_` 子命令 `"relay-probe"` 臂）：`AERO_RELAY_PROBE_BIN` env 覆盖（缺省 `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`）；可选 `[mock-url]` 透传；`stdout/stderr` inherit；`timeout(120s)` + `child.kill()`；码映射 `0→ok / 1→error / 2→warning(2) / 其他→error`。**该臂不改**（acceptance AC5：legacy 无 contract break）——`--relay` 模式在 aero-eng 内**复用其 spawn 契约**（同 env、同 bin、同超时），但改 captured-output + PASS 行断言（R5） |
| E4 | `crates/aero-audit-connector/tests/state_machine.rs` + `tests/claim_validation.rs`（iss/aud/scope/sub validation before POST——'scope audit:event:write' claim 契约） | ✅ 全中：`state_machine.rs` = **8** 个 async 测试（lease 过期重领+token 轮换、settle 移出 claimable、backoff 序列、422/409 permanent→attempt1 requeue/attempt2 dead、403→attempt1 即 dead、stale-token 三 fence 全 false、skew≥lease 不活锁、signature-rejected dead）；`claim_validation.rs` = 27 个测试条目（wrong_issuer / missing_audience / missing_audit_scope / wrong_subject / token_without_client_id 等全部 **POST 前拒绝**）。claim 契约值：`expected_scope = "audit:event:write"`（= `client.rs::SCOPE_AUDIT`）、`expected_aud = "audit-governance"`、`expected_iss`/`expected_sub` 同值锁步。**probe 场景名/断言即这些测试的黑盒镜像**（E6） |
| E5 | `crates/aero-audit-connector/src/client.rs`（DeliveryError classes: 422/409/403 permanent vs transient） | ✅ 全中：`pub enum DeliveryError { Transient(anyhow::Error), Permanent(PermanentKind), Forbidden }`；`PermanentKind::{Unprocessable(422), Conflict(409), ReceiptMismatch, PayloadGuard, SignatureRejected}`；`Forbidden` = HTTP 403 立即 dead（relay.rs `deliver_claim` Forbidden 臂不经 `is_dead_at`）；422/409 走 Permanent 臂（attempt1 requeue / attempt≥2 `mark_dead`）。**probe 的 422→dead 语义源头** |
| E6 | ⚠️ 「DB-free mock-sink probe（aero-audit-relay-probe, covering 422→dead / 403 immediate-dead / claim validation / lease）**exists**」 | ⚠️ **历史缺失记录已 superseded**：2026-08-08 的 file-gate 确实因 probe 未落地而 SKIP；当前 `aero-audit-relay-probe.rs` 已存在，直接运行与 `relay-mock-probe` harness 均为 PASS。9 个场景名、PASS 行格式与退出码契约保持不变；本 direction 仍只消费该 sibling bin，不重复实现。 |
| E7 | ⚠️ 「Extends … scripts/test-integration.sh leg C」 | ✅→⚠️ **事实更正：leg C 已存在** = dead-row 腿（"audit-provision-check leg C (one dead row ⇒ fail-closed, never delivered)"），跑在 T-11 throwaway 库上（relay enabled + binding 的 leg-D 态之后、`drop_created_database` 之前），grep `dead=1` / `delivered=0` / `audit-provision-check: dead:` / `verdict: fail-closed`，复用 `b5_check "audit-provision-check" "PASS"`。`b5_check` **append** 到 B5_LOG；`assert_b5_contract_pin` 对每个 executed slot 只要求 `grep -Eq "^B5-CHECK ${entry}: (PASS|SKIP)( |$)"` ≥1 行——**同 slot 多行合法**。新 stuck-lease 腿 = **追加腿 leg E**（同 slot 复用，48/48 不变，`b5-pin.sh` 零改动） |
| E8 | （补充核对）0239 DDL claim-state CHECK + `claim_due` 重领过滤 | ✅ `migrations/0239_audit_governance_outbox.sql`：`status INTEGER NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))`；`claim_token UUID`、`lease_expires_at TIMESTAMPTZ` + CHECK `(claim_token IS NULL AND lease_expires_at IS NULL) OR (claim_token IS NOT NULL AND lease_expires_at IS NOT NULL)`——**种子 status=1 行必须同时设 claim_token + lease_expires_at**（leg E 种子约束）。`pg.rs claim_due` 过滤：`status IN (0,1) AND available_at <= clock_timestamp() AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())`——**lease 过期的 claimed 行可被活 relay 下个 tick 重领**；故「claimed 行 age 超 MAX_LEASE_SECONDS」⇒ 无活 relay 在重领 ⇒ relay 实质上已死（stuck-lease 语义链成立）。`available_at` 在 claim 时不改写（claim 只铸 lease_expires_at + claim_token + attempts+=1）——Q7 以 available_at 计龄是保守代理：claim_time ≥ available_at ∧ lease ≤ 86400 ⇒ age(from available_at) > 86400 在 prompt-claiming 下必已 lease 过期 |
| E9 | （补充核对）aero-eng 依赖/尺寸/测试面约束 | ✅ `crates/aero-eng/src/checks.rs` `ALLOWED_DEPS`：`("aero-eng", &["aero-common", "serde", "tokio"])`——**aero-eng 不得依赖 aero-audit-connector**（加依赖须改 checks.rs + dependency-check.sh，出范围）；MAX_LEASE_SECONDS 用本地 clone 字面量。`audit_provision.rs` 现 **790 行**（800 WARN / 1200 HARD，`scripts/file-size-check.sh`：WARN 不 exit 非零，但 sibling B5-1 §6 已立「不得新增 WARN」纪律）——`run_relay` + 校验器 ≈ 130 行**必须落兄弟模块**（R10）。`tests/audit_provision.rs`（487 行）用 `use aero_eng::audit_provision::*;` **glob import**——`pub use` 再导出对测试透明。`cli_smoke.rs`/`cli_integration.rs` 均不引用 audit-provision-check/relay（只测 help/unknown-command/doctor/gate-list 等泛化面）——main.rs 增 `--relay` 臂不影响 cli_smoke |

### 1.1 核对偏差汇总（direction/evidence vs 实况）

| 项 | direction 声称 | 实况（2026-08-08 实读） | 影响 |
|---|---|---|---|
| probe bin 存在性 | 「DB-free mock-sink probe（aero-audit-relay-probe …）exists」 | **历史记录（已 supersede）**：当时不存在；当前 bin 已落地，直接 probe 与 relay-mock leg 均 PASS | `--relay` 仍锚定设计文档 9 个具名场景；file-gate 保留为防御性检查，当前分支执行 PASS |
| leg C | 「Extends … leg C」 | leg C **已存在**（dead-row 腿）；新腿是追加 leg E，同 slot 复用（E7） | 新增腿不改动既有 leg C 语义与 grep |
| 种子 status=1 | 「seed a claimed row with clock_timestamp()-based available_at」 | 0239 CHECK 强制 claim_token 与 lease_expires_at 同存（E8） | leg E 种子 SQL 必须三列齐设（R8.3） |
| MAX_LEASE_SECONDS 引用 | evidence 指向 connector relay.rs | aero-eng 依赖审计禁止 import（E9） | 本地 `const MAX_LEASE_SECONDS: i64 = 86_400` clone + 交叉引用注释（R4） |
| verdict 面 | 「flips the verdict to fail-closed」 | Q7 数据进 base `verdict()` 输入（新 stuck-lease 支）——dead-first 与 relay-off+undelivered 优先级**逐字节保留**（R3） | base `run()` 也获得 stuck-lease 信号（AC1），`--relay` 继承之（AC3） |

## 2. Verified current state

```
已存在（E1-E9，全部 verified）：
  Q0-Q5 psql 查询面（relay 开关/bindings、v1 桶、0239 四 status 桶、oldest-pending-age、dead 明细）
  verdict() 三态矩阵（dead-first → relay-off+undelivered → consistent/healthy）
  --priority 面（B5-3）+ run_priority 的 direct-spawn/码映射/120s 超时先例
  network relay-probe CLI 臂（E3，预埋 file-gate；当前 probe bin 存在时 exit 0）
  harness leg B1/B2（AUDIT_PROVISION_DB）、leg D/C（T-11 库，relay-on healthy → dead fail-closed）
  relay-mock-probe harness leg（file-gate + 9 行 PASS grep，当前 PASS）+ b5-pin.sh 当前 48-slot（27 个可执行 slot + 21 个 [PROPOSED]）
  connector 状态机全实现（relay.rs/client.rs/pg.rs）+ 35 个 in-crate/集成测试（E4/E5）

缺口（本 direction 关闭，全部 verified）：
  a) 门只报 oldest *pending* age（Q4，status=0）——relay claim 后死亡 ⇒ claimed 行
     无 CLI 信号，静默老化向 lease 过期（86400s），「grant only after relay works」(B5-4) 未真验证
  b) B5-2 状态机 probe 与供给 verdict 零关系：probe 走独立 network relay-probe 面，
     audit-provision-check 的 Healthy 不要求任何 probe 证据
  c) probe 缺席时 --relay 仍须 fail-closed（防御性语义）；当前 probe 已落地并通过
```

## 3. Scope

**In scope**：
- 基础 `run()` 内 Q7：最老 claimed 行年龄（`status = 1 AND claim_token IS NOT NULL`，计龄自 `available_at`，E8 语义链）+ 报告行 + `verdict()` 新 stuck-lease fail-closed 支（R1-R3）
- `G0239Counts` 增 `oldest_claimed_secs: Option<i64>`（快照承载，R2）；`MAX_LEASE_SECONDS` 本地 clone 常量（R4）
- 新 `--relay` 模式：`run_relay()` = base check（含 Q7）→ probe spawn（captured-output + 9 具名 PASS 行 + 零 FAIL 行断言）→ 聚合 verdict（R5-R6）
- main.rs `AuditProvisionCheck_` 臂增 `--relay` + usage/help 文本（R7）
- harness 追加腿：leg E（stuck-lease fail-closed，base 模式）、leg F（--relay healthy + 9/9 PASS，file-gated）、leg F′（--relay + stub bin 负向，exit 1）（R8）
- `crates/aero-eng/tests/audit_provision.rs` 扩展：claimed-age 解析/报告/verdict 用例 + probe 校验器用例 + `G0239Counts` 构造点机械补字段（R9）
- 新兄弟模块 `crates/aero-eng/src/relay_runtime.rs` + `ConnParams`/`parse_db_url` 搬迁（尺寸纪律，R10）

**Out of scope**：
- **`aero-audit-relay-probe` bin 本体的重复实现**——它已由 sibling direction 落地；本 direction 只按 E6 契约消费（9 具名场景、`probe: <name>: PASS` 行格式、退出码 0/1/2）。缺席时的 fail-closed 分支仍保留。
- connector `src/` 任何改动、connector `Cargo.toml`、0239/0240/0241 迁移——零改动
- `verdict()` 既有三支优先级（dead-first → relay-off+undelivered）**逐字节保留**——stuck-lease 是纯追加支（R3）
- legacy `network relay-probe` 臂——零改动（AC5 no contract break）
- `--priority` 面、`Gate_`/`scripts/relay-mock.sh`、b5-pin.sh 当前 48-slot 清单、新 B5-CHECK slot——零改动（E7）
- aero-eng 依赖图（ALLOWED_DEPS）——零改动（R4 clone 字面量替代 import，E9）

## 4. Requirements

### R1 — Q7 SQL + 解析（最老 claimed 行年龄）

`crates/aero-eng/src/relay_runtime.rs`（新模块，R10）内：

```rust
/// Q7 — oldest claimed-row age in seconds (B5-2 runtime-health leg): a
/// claimed row (status=1 with a fencing token) whose age from `available_at`
/// exceeds MAX_LEASE_SECONDS has certainly outlived its lease under prompt
/// claiming — `claim_due` reclaims expired-lease rows every tick, so an old
/// claimed row means no live relay is running. `available_at` is not
/// rewritten at claim time; the lease is minted at claim >= available_at and
/// clamped to <= MAX_LEASE_SECONDS (aero-audit-connector/src/relay.rs).
const Q7_SQL: &str = "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 1 AND claim_token IS NOT NULL";

/// Parse the Q7 single-value line; empty output (no claimed rows) -> None.
pub fn parse_claimed_age(out: &str) -> Result<Option<i64>, String>
```

- `{table}` 只替换为 [`G0239_CANDIDATES`] 解析出的固定候选字面量（Q3/Q4/Q5 同款 `replace` 路径，非用户输入）。
- 空输出 → `None`（无 claimed 行）；非空非整数 → `Err`（`outcome_error`，exit 1——与 Q4 同契约，门一旦跑 Q7 就不静默跳过）。
- 触发面：**基础 `run()`** 的 `Some(table)` 分支内，Q4 之后（表缺席 → 不跑、不印行，现有 `outbox-0239: not migrated` 覆盖）。`run_priority` 先跑 `run()` 自动继承。

### R2 — 快照承载 + 报告行

- `G0239Counts`（audit_provision.rs）增 `pub oldest_claimed_secs: Option<i64>`（None = 无 claimed 行）。`run()` 的 `G0239Counts` 构造点补值；`tests/audit_provision.rs` 现有构造点机械补 `oldest_claimed_secs: None`（测试文件本就在扩展范围，R9）。
- `format_report` 在 `Some(g)` 分支、`oldest-pending-age` 行之后追加（Q4 同款形态）：

```
audit-provision-check: oldest-claimed-age: 42s      # Some(secs)
audit-provision-check: oldest-claimed-age: n/a      # None（无 claimed 行）
```

- 行格式与 Q4 的 `oldest-pending-age: Ns` 对称（harness grep 面）。行渲染函数 `pub fn claimed_age_line(secs: Option<i64>) -> String` 落 relay_runtime.rs，由 `format_report` 调用（truth-check 零调用守卫：必须被调用，不加 allowlist）。

### R3 — verdict() stuck-lease fail-closed 支（纯追加，优先级不动）

`verdict()` 在既有两检查之后、`Verdict::Healthy` 之前追加：

```rust
// Q7 stuck lease: a claimed row older than the lease bound means the relay
// died after claiming (a live relay reclaims expired-lease rows each tick —
// pg.rs claim_due filter). Fail-closed; never folded into pending backlog.
if let Some(g) = &s.g0239 {
    if let Some(secs) = g.oldest_claimed_secs {
        if secs > MAX_LEASE_SECONDS {
            return Verdict::FailClosed(format!(
                "stuck lease: oldest claimed row is {secs}s old (> {MAX_LEASE_SECONDS}s); relay died after claiming"
            ));
        }
    }
}
```

- 优先级顺序（AC4 原句，逐字节保留）：① dead > 0 → fail-closed（dead 是终态，**压过一切**）；② relay 关 + undelivered > 0 → fail-closed（`no audit:event:write grant issued` 文案与既有 leg B2 grep 不变）；③ **新** stuck-lease → fail-closed；④ 其余 → consistent/healthy。
- 阈值 `secs > MAX_LEASE_SECONDS`（严格大于；== 86400 不 fail——保守，避免时钟抖动误伤；E8 语义链保证 > 86400 必已 lease 过期）。
- 文案前缀保持 `verdict: fail-closed —`（harness grep `verdict: fail-closed` 不破）。
- **stuck-lease 支在 base `run()` 即生效**（Q7 是快照输入，非 --relay 专属）——AC1 原句「flips the verdict to fail-closed」按此落地；`--relay` 继承之（R5 先跑 base）。

### R4 — MAX_LEASE_SECONDS clone 常量（零依赖）

`relay_runtime.rs`：

```rust
/// Lease upper bound, cloned from aero-audit-connector/src/relay.rs
/// (normative source; aero-eng cannot depend on the connector crate —
/// checks.rs ALLOWED_DEPS). Kept in lockstep by the Q7 stuck-lease test.
pub const MAX_LEASE_SECONDS: i64 = 86_400;
```

- 禁止新增 `aero-audit-connector` 依赖（E9：ALLOWED_DEPS `("aero-eng", &["aero-common", "serde", "tokio"])`；加依赖须改 checks.rs + dependency-check.sh = 出范围）。
- 本仓 clone 常量先例：connector relay.rs 自身即 "clone of `AiUsageRepo::MAX_LEASE_SECONDS`"。
- 锁步测试：`assert_eq!(MAX_LEASE_SECONDS, 86_400)`（R9）——阈值契约漂移即红。

### R5 — `run_relay()`：base check + probe spawn + captured-output PASS 行断言

`relay_runtime.rs`：`pub async fn run_relay(db_url: &str) -> Outcome`。

**阶段 1 — base check（fail-closed 优先级在 probe 之前）**：调用 `audit_provision::run(db_url)`（打印 base 报告含 Q7 行与 verdict）。`is_error()` → 直接返回（**dead / relay-off+undelivered / stuck-lease 任一 fail 即短路，probe 不 spawn**——AC4 原句）。

**阶段 2 — probe spawn**（复用 E3 臂的 spawn 契约，唯一差别 = captured output）：
- bin 解析：`AERO_RELAY_PROBE_BIN` env 覆盖；缺省 `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`。
- `stdout`/`stderr` 均 `Stdio::piped`（捕获后回显）；`timeout(120s)` + `child.kill()`（臂同款）。
- 子进程结束后：stdout/stderr **原样回显到本命令的 stdout/stderr**（`probe: <name>: PASS` 行保持 harness greppable）。
- spawn 失败（bin 缺席 → cargo exit 101 或 `AERO_RELAY_PROBE_BIN` 指向不存在路径）→ `Outcome::error("cannot launch relay probe: {e}")`——**fail-closed**（门无法验证 B5-2 即不 Healthy；harness 腿 F 以 file-gate 保证 phase-1 窗口绿，R8）。

**阶段 3 — 校验（captured-output 断言，不信任 exit code 单点）**：`validate_probe_output(exit_ok: bool, stdout: &str) -> Result<(), String>`（R6）：
- 子进程退出码映射（复用臂契约）：`0 → exit_ok=true`；`1 → error("relay probe: one or more scenarios FAILED")`；`2 → error("relay probe usage error — cannot verify B5-2 state machine")`（比臂的 warning(2) 更严——`--relay` 是 verdict 门，非 probe 启动器）；其他 → error abnormal。
- `validate_probe_output` Ok → 打印 `audit-provision-check: relay-probe: PASS (9/9)`，`Outcome::ok`。
- Err(reason) → 打印 `audit-provision-check: relay-probe: FAIL — {reason}`，`Outcome::error`。
- **base 为 Consistent（relay 关、零 undelivered）时 probe 仍跑**：probe 是 DB-free 状态机验证（E6），「完整前置条件一条命令」要求它必须可验证——probe FAIL 同样 error（fail-closed 门语义）。

### R6 — probe 输出校验器（纯函数 + 9 场景常量，单点定义）

`relay_runtime.rs`：

```rust
/// The B5-2 probe's nine named scenarios — the harness relay-mock-probe leg
/// greps `^probe: .*: PASS$` count == 9; the names are the contract from
/// docs/design/2026-08-08-aero-cli-b5-2-relay-probe-bin-landing.design.md
/// §2.1 (probe PASS lines carry no trailing detail).
pub const RELAY_PROBE_SCENARIOS: [&str; 9] = [
    "happy_path", "forbidden_403", "permanent_422", "permanent_409",
    "receipt_mismatch", "transient_500", "transient_timeout",
    "lease_invariant", "fencing_stale_token",
];

/// Ok iff: child exit 0; every REQUIRED scenario has a line-anchored
/// `probe: <name>: PASS`; and no `probe: …: FAIL` line is present.
pub fn validate_probe_output(exit_ok: bool, stdout: &str) -> Result<(), String>
```

- **逐行锚定**：`probe: {name}: PASS` 必须是整行（`^probe: <name>: PASS$`）——`PASS` 行带尾随 detail 即 Err（E6 契约：harness `^probe: .*: PASS$` 锚定行尾）。
- 缺失场景 → Err 指明第一个缺失名字（如 `missing probe PASS line: probe: lease_invariant: PASS`）。
- 任一 `^probe: .*: FAIL` 行 → Err（belt-and-braces：probe 未来加场景并 FAIL 时，即使 9 个必选全 PASS 也红）。
- 未知名字的 PASS 行**不罚**（probe 可增长场景；契约只要求 9 个必选）。
- 纯函数 + 总函数，单测全覆盖（R9）；被 `run_relay` 调用（truth-check 零调用守卫）。

### R7 — main.rs `--relay` 臂接线

`crates/aero-cli/src/main.rs` `AuditProvisionCheck_`：
- 臂匹配增 `Some("--relay") => aero_eng::relay_runtime::run_relay(&url).await`。
- usage 错误文案 `"usage: audit-provision-check [--priority|--relay]"`（未知 flag 仍 loud error——静默跑 base 会跳过 probe，同 `--priority` 的 anti-typo 纪律）。
- help/description 文本追加 `; --relay = B5-2 relay state-machine probe (spawns aero-audit-relay-probe; Healthy requires all 9 probe PASS lines)`。
- `--priority` 臂、`network relay-probe` 臂、cmds 串：零改动。cli_smoke 不引用本命令（E9）→ 保持绿。

### R8 — harness 追加腿（`scripts/test-integration.sh`，T-11 块内，零新 slot）

全部在既有 T-11 throwaway 库块内（leg D 之后、leg C 之前追加；leg C 语义与 grep 逐字节不动），`b5_check "audit-provision-check" "PASS"` 同 slot 复用（E7：append 多行 + pin 只要求 ≥1 行，48/48 不变）：

1. **leg F′（--relay 负向，stub bin，无 file-gate）**——leg-D 态（relay on + binding + T-11 遗留 pending 行 = Healthy）：
   `AERO_RELAY_PROBE_BIN=/bin/true cargo run -p aero-cli -- audit-provision-check --relay` ⇒ **exit 非零** + stdout 含 `relay-probe: FAIL`（/bin/true exit 0 但零 PASS 行 → 校验器 Err——证明不信任 exit code 单点，AC2 的 stub-bin 注入面）。
2. **leg F（--relay healthy，file-gate）**——同态：
   `if [ -f "crates/aero-audit-connector/src/bin/aero-audit-relay-probe.rs" ]`（probe 缺席 → `echo` 显式 SKIP 文案 + 不 FAIL，同 relay-mock-probe leg 先例）；probe 在场时 `audit-provision-check --relay` ⇒ exit 0 + `relay-probe: PASS (9/9)` + `grep -c '^probe: .*: PASS$' == 9`。
3. **leg E（stuck-lease fail-closed，base 模式，无 file-gate）**——leg F′/F 之后、leg C 之前：
   - 种子（E8 CHECK 约束，三列齐设）：
     `INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload, available_at, claim_token, lease_expires_at) VALUES (gen_random_uuid(), 1, 'message', 10, jsonb_build_object('event_id', gen_random_uuid()::text, 'source_system', 'aero-im.source'), clock_timestamp() - interval '2 days', gen_random_uuid(), clock_timestamp() - interval '1 hour');`
   - `audit-provision-check`（base 模式）⇒ **exit 非零** + grep `oldest-claimed-age: [0-9]{6,}s`（种子 2 天 = 172800s > 86400；6+ 位数字即必超阈值，格式 `Ns`）+ `verdict: fail-closed` + `stuck lease`。
   - **relay 已 enabled + binding 在场**（leg-D 态）——否则 relay-off 支先报 `no audit:event:write grant issued`，stuck-lease 理由不可达（R3 顺序）；grep 到 stuck-lease 理由即证明优先级正确。
4. 顺序纪律：F′ → F → E → 既有 C（C 的 `UPDATE … WHERE status = 0 LIMIT 1` 不受 E 的 status=1 行影响；E 的行使 C 的 `dead=1` 断言不受影响——E 行保持 status=1）。

### R9 — 单测（`crates/aero-eng/tests/audit_provision.rs` 扩展）

- **claimed-age 解析器**（`parse_claimed_age`）：`"42"` → `Some(42)`；空串 → `None`；`"abc"` → `Err`。
- **报告行**：`claimed_age_line(Some(42))` == `"audit-provision-check: oldest-claimed-age: 42s"`；`None` == `"…n/a"`；`format_report` 集成——带 `oldest_claimed_secs` 的快照含该行；`g0239: None` → 无该行（`report_not_migrated_has_no_age_or_dead_lines` 同款追加）。
- **verdict 用例**：① relay on + `oldest_claimed_secs: Some(90_000)` → FailClosed 且 reason 含 `stuck lease`；② dead=1 + stuck lease 同场 → dead 理由（dead-first 压过）；③ relay off + undelivered + stuck lease 同场 → `no audit:event:write grant issued`（优先级 ② 压过 ③）；④ `Some(86_400)`（== 阈值）→ Healthy（严格大于）；⑤ `Some(1)` → Healthy；⑥ `None` → Healthy 不变。
- **校验器**（`validate_probe_output`）：9/9 行 → Ok；`exit_ok=false` → Err；9 行缺 `lease_invariant` → Err 指明缺失名；PASS 行带尾随 detail → Err；9/9 PASS + 一条 `probe: x: FAIL boom` → Err；空输出 → Err；`RELAY_PROBE_SCENARIOS.len() == 9` + `MAX_LEASE_SECONDS == 86_400` 锁步断言。
- 既有 `G0239Counts` 构造点机械补 `oldest_claimed_secs: None`；既有 verdict/报告测试**零语义改动**（AC4 的测试面证明）。
- cli_smoke 零改动（E9：不引用本命令；main.rs 改动为纯增量）。

### R10 — 尺寸纪律（文件拆分 + 透明搬迁）

- `audit_provision.rs` 现 **790 行**（800 WARN / 1200 HARD，`scripts/file-size-check.sh`）；B5-1 sibling 已立「不得新增 WARN」纪律（E9）。
- 新生产代码落**兄弟模块** `crates/aero-eng/src/relay_runtime.rs`（`lib.rs` 增 `pub mod relay_runtime;`）：Q7_SQL、`parse_claimed_age`、`claimed_age_line`、`MAX_LEASE_SECONDS`、`RELAY_PROBE_SCENARIOS`、`validate_probe_output`、`run_relay`。
- **搬迁保尺寸**：`ConnParams` + `parse_db_url`（audit_provision.rs 内自包含 ~55 行，被 `run()`/`run_priority()` 使用）搬迁至 relay_runtime.rs，audit_provision.rs 以 `pub use crate::relay_runtime::{ConnParams, parse_db_url};` 再导出——公开 API 不变，`tests/audit_provision.rs` 的 glob import（`use aero_eng::audit_provision::*;`）透明解析（E9）。in-file 增量（G0239Counts 字段 + Q7 查询接线 + 报告行 + verdict 支 ≈ 25 行）后保持 ≤800。
- 目标 relay_runtime.rs ≤ 250 行；`unreachable_pub = "warn"`（root Cargo.toml）——被集成测试/CLI 消费的项 `pub`，纯内部项私有。tests 文件（487 行 + ~120）远低于 800 WARN。

## 5. Testable acceptance mapping（direction acceptance 原句保留，re-ground 到当前仓态）

| AC（原句） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **AC1** 新 Q7：oldest claimed-row age in seconds（status=1, claim_token IS NOT NULL）；报告行 `oldest-claimed-age: Ns`——claimed 行 age > MAX_LEASE_SECONDS 翻 fail-closed；种子 claimed 行（clock_timestamp()-based available_at）验证 | SQL = `SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 1 AND claim_token IS NOT NULL`（R1）；行 `audit-provision-check: oldest-claimed-age: Ns` / `n/a`（R2）；阈值 `> 86_400` → FailClosed `stuck lease`（R3）；单测 ⑥ 组 + harness leg E 种子（`clock_timestamp() - interval '2 days'` + claim_token/lease_expires_at 齐设，E8 CHECK）→ exit 1 + `oldest-claimed-age: [0-9]{6,}s` + `stuck lease`（R8.3） | R1/R2/R3/R8/R9 |
| **AC2** 新 `--relay` 模式：base check + spawn aero-audit-relay-probe（honoring AERO_RELAY_PROBE_BIN）with captured-output requirement——全部 `probe: <name>: PASS` 行在场才允许 Healthy（proposed wiring：复用 aero-cli 臂契约，exit 0=all PASS, 1=any FAIL, 2=usage） | `run_relay` = base（error 短路）→ spawn（AERO_RELAY_PROBE_BIN / 缺省 cargo run / 120s / piped）→ `validate_probe_output`（9 具名行 + 零 FAIL，R6）→ `relay-probe: PASS (9/9)` / `FAIL — reason`（R5）；leg F′ stub-bin 注入（`/bin/true` exit 0 零 PASS 行 → exit 1 + `relay-probe: FAIL`）证明不信任 exit code（R8.1）；leg F 实 probe 9/9（file-gated，R8.2） | R5/R6/R8 |
| **AC3** Healthy = relay 开关 on + bindings>0 + 无 dead + 无 stuck lease + 全部 B5-2 probe PASS——完整「grant only after relay works」前置条件一条命令 | `--relay` 阶段链：base Healthy（含 Q7 无 stuck）→ probe 9/9 → exit 0；任一环节 fail → exit 1（leg F 全链正例 + leg E/F′ 各负例）；verdict() 单测 ①③④⑤⑥ 钉优先级与阈值边界 | R3/R5/R8/R9 |
| **AC4** fail-closed 优先级不变：dead 仍压一切；relay-disabled + undelivered 仍在任何 probe 之前 fail | verdict() 既有两支逐字节保留（R3）；`run_relay` 阶段 1 error 短路（R5——probe 不 spawn）；单测 ②③（dead 压 stuck、relay-off 压 stuck） | R3/R5/R9 |
| **AC5** 扩展 tests/audit_provision.rs（claimed-age 解析器 + verdict 用例）+ test-integration.sh leg C（throwaway 库）；legacy `network relay-probe` 保持可用（no contract break） | 新解析器/报告/verdict/校验器用例（R9）；harness leg E/F/F′ 追加于既有 T-11 块（R8），leg C 语义与 grep 零改动（E7）；`network relay-probe` 臂零改动（E3）——probe 落地前后行为不变（缺 bin 时 exit 1 同现状）；`cargo test --workspace --lib` + `cargo clippy --workspace --all-targets`（无新警告）绿 | R7/R8/R9/R10 |

## 6. Coordination & hard rules（AGENTS §4）

- **probe bin 边界**：`aero-audit-relay-probe.rs` 已由 sibling direction 落地——本 direction 只消费其设计契约（E6 9 场景名 + `probe: <name>: PASS` 行格式 + 退出码 0/1/2）；`--relay` 在 probe 缺席时仍 fail-closed（spawn error），harness 腿 F 的 file-gate 是防御性检查，当前分支为 PASS。
- **零新 slot / 零 b5-pin 改动**：leg E/F/F′ 全部复用 `audit-provision-check` B5-CHECK slot（`b5_check` append + pin ≥1 行，E7）；48/48 保持。
- **零新依赖**：`MAX_LEASE_SECONDS` clone 字面量 + 锁步测试（R4）；ALLOWED_DEPS / checks.rs / dependency-check.sh 零改动。
- **fail-closed 顺序**：dead → relay-off+undelivered → stuck-lease → probe；任一 fail 即短路，probe 绝不先于 DB-fail 状态 spawn（R5）。
- **种子 CHECK 纪律**：status=1 种子行必须 claim_token + lease_expires_at 齐设（E8 CHECK）；`available_at` 用 `clock_timestamp()` 系（单时钟域，E8）。
- **grep 精确性**：新行 `oldest-claimed-age:` 与既有 `oldest-pending-age:` 并存——harness/文档 grep 用完整前缀（`audit-provision-check: oldest-claimed-age:`）；`relay-probe: PASS (9/9)` 与 probe 自身 `probe: <name>: PASS` 行并存——后者以 `^probe: ` 锚定。
- **尺寸纪律**：`audit_provision.rs` 保持 ≤800（R10 搬迁）；新代码落 `relay_runtime.rs`；tests 文件 ≤800。
- **迁移纪律**：本 direction 零迁移；0239/0240/0241 已落地勿改。
- **活验证**：leg E/F/F′ 内建 throwaway 库纪律（T-11 块既有 `create_throwaway_database`/migrate/`drop_created_database`，AGENTS §4.3）。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/truth-check.sh`（新函数有调用，零 allowlist 新增）· `scripts/{file-size-check,web-check}.sh`（`audit_provision.rs` 不得新增 WARN，R10）· `cargo test -p aero-audit-connector --all-targets`（既有 24 passed + 1 ignored 基线不回归——本 direction 零 connector 改动）。
