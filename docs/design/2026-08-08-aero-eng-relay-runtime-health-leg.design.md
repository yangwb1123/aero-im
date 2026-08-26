# Design — aero-eng：relay runtime-health 腿（Q7 stuck-lease + `--relay` 统一 probe 门）

- **落家**: `crates/aero-eng`（`relay_runtime.rs` 新兄弟模块 + `audit_provision.rs` 增量 + `aero-cli/src/main.rs` 臂增量 + harness 追加腿 E/F/F′ + `tests/audit_provision.rs` 扩展）
- **Requirements**: `docs/requirements/2026-08-08-aero-eng-relay-runtime-health-leg.req.md`（253 行，R1-R10）
- **宿主命令**: `audit-provision-check`（`crates/aero-cli/src/main.rs:488-510` 命令臂）
- **Sibling 设计**: `docs/design/2026-08-08-aero-eng-b5-1-operation-class-coverage-leg.design.md`（同模块 Q6 腿，尺寸纪律/报告行纪律先例）、`docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`（命令本体）、`docs/design/2026-08-08-aero-cli-b5-2-relay-probe-bin-landing.design.md`（**probe bin 契约源：9 场景名 + `probe: <name>: PASS` 行格式 + 退出码 0/1/2**）、`docs/design/2026-08-07-aero-cli-b5-acceptance-gate-harness.design.md`（leg 纪律）
- **Status**: Implemented（设计证据全部经源码实读复核，见 §1；Q7/`--relay`/harness 已于 2026-08-20 通过完整 integration 验收）
- **当前状态更新（2026-08-20）**：本设计中的 `relay_runtime.rs`、Q7、`audit-provision-check --relay` 以及 T-11 F′/F/E 腿均已落地。§1 的 E6“probe 缺失”是实现前历史快照；当前 sibling probe 已存在并输出 9/9 PASS。
- **Current B5 pin (2026-08-26)**：live `B5_CONTRACT_TEST_LIST` 为 48 个 slot（27 个非 `[PROPOSED]` 可执行 slot + 21 个 `[PROPOSED]` 仓外占位）；“executed”是 manifest 分类，不是本设计声称已经运行的测试数。

## §1 证据核验（untrusted claims → 源码逐条对照）

| # | 声称 | 核验结果 |
|---|---|---|
| E1 | `audit_provision.rs` Q4_SQL 只查 status=0；`G0239Counts` 仅有 `oldest_pending_secs`；`verdict()` 无 lease/claimed-age 输入 | ✅ 全中。`Q4_SQL` :52 `… WHERE status = 0`；`G0239Counts` :92-105 字段 = table/pending/claimed/delivered/dead/oldest_pending_secs/dead_rows，全文件零 `status = 1` 年龄查询；`verdict()` :134-155 三支（dead-first :136-139 → relay-off+undelivered :143-152 → Consistent/Healthy :152/:154），`undelivered = v1.pending + v1.claimed + g0239.pending + g0239.claimed`，无 lease 输入。缺口本体成立 |
| E2 | `aero-audit-connector/src/relay.rs` MAX_LEASE_SECONDS=86400 / MAX_BACKOFF_SECONDS=300 / PERMANENT_DEAD_AT=2 / is_dead_at / clamped_lease / audit_backoff | ✅ 符号全中：:31 `MAX_LEASE_SECONDS: i64 = 86_400`（注释 "clone of `AiUsageRepo::MAX_LEASE_SECONDS`" = 本仓 clone 常量先例）、:33 MAX_BACKOFF_SECONDS、:42 PERMANENT_DEAD_AT=2、:48 `is_dead_at(attempts) = attempts >= 2`、:67 `clamped_lease` clamp [1, 86400]、:56 `audit_backoff` = 2^(attempts−1) cap 300 |
| E3 | `aero-cli/src/main.rs` relay-probe 臂：直接 spawn、`AERO_RELAY_PROBE_BIN`、0/1/2 映射、120s | ✅ 全中（:428-481）：env 覆盖缺省 `cargo run --quiet -p aero-audit-connector --bin aero-audit-relay-probe`；可选 `[mock-url]` 透传 :448-450；`Stdio::inherit` :451-452；`timeout(120s)` + `child.kill()` :454-480；映射 `0→ok / 1→error / 2→warning(2) / 其他→error`。**该臂零改动**（AC5） |
| E4 | `state_machine.rs` 8 测试 + `claim_validation.rs` 27 条目（iss/aud/scope/sub POST 前拒绝） | ✅ 全中：`state_machine.rs` 恰 **8** 个 `#[tokio::test]`（:83 stale_token_cannot_ack_after_reclaim / :136 backoff_is_bounded_and_exponential / :169 permanent_error_dead_after_exactly_two_attempts / :255 forbidden_dead_on_first_attempt / :286 happy_path_settles_and_removes_from_claimable / :321 skew_gt_lease_cannot_livelock_claim_fence_settle / :360 priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane / :470 signature_rejected_dead_after_exactly_two_attempts）；`claim_validation.rs` = 26 async + 1 sync（`jwt_claims_decode_roundtrip` :364）= **27** 条目，wrong_issuer/missing_audience/missing_audit_scope/wrong_subject 等全部 **POST 前拒绝**（:139-186）；契约值 `expected_scope = "audit:event:write"`（= `client.rs:47 SCOPE_AUDIT`）、`expected_aud = "audit-governance"` :29 |
| E5 | `client.rs` DeliveryError：422/409 permanent、403 immediate-dead | ✅ 全中：`PermanentKind::{Unprocessable, Conflict, ReceiptMismatch, PayloadGuard, SignatureRejected}` :53-65、`Forbidden` :80；HTTP 映射 :220 FORBIDDEN→Forbidden、:222 422→Unprocessable、:225 409→Conflict |
| E6 | **历史核对**：`aero-audit-relay-probe` bin 当时**不存在**（src/bin/ 仅 3 drill）；harness relay-mock-probe leg 是 file-gate（`grep -c '^probe: .*: PASS$' == 9`）；当前 48-slot（27 个可执行 slot + 21 个 [PROPOSED]）含 relay-mock-probe | ✅ 历史事实已被当前实现 supersede：`aero-audit-relay-probe.rs` 已落地，9 场景与 PASS 行均通过；file-gate 仍作为防御性检查保留，当前 48-slot 不变（27 个可执行 + 21 个 [PROPOSED]）。 |
| E7 | leg C 已存在（dead-row 腿，T-11 库）；`b5_check` append；pin 只要求 ≥1 行 | ✅ 全中：`test-integration.sh:421-448` leg C（`UPDATE … SET status = 3 WHERE event_id = (SELECT … WHERE status = 0 LIMIT 1)` + grep `dead=1`/`delivered=0`/`audit-provision-check: dead:`/`verdict: fail-closed` + `b5_check "audit-provision-check" "PASS"`）；`b5-pin.sh:73-79` `b5_check` = echo + append 到 B5_LOG；`assert_b5_contract_pin` :100-106 `grep -Eq "^B5-CHECK ${entry}: (PASS|SKIP)( |$)"`（≥1 行即可，**同 slot 多行合法**）。leg D 在 :379-417（relay enabled + binding ⇒ healthy + `oldest-pending-age:` grep），插入点 = leg D 结束与 leg C 之间 |
| E8 | 0239 CHECK 强制 claim_token+lease_expires_at 同存；`claim_due` 重领过期 lease 行 | ✅ 全中：`migrations/0239_audit_governance_outbox.sql` `CONSTRAINT audit_governance_claim_state CHECK ((claim_token IS NULL AND lease_expires_at IS NULL) OR (claim_token IS NOT NULL AND lease_expires_at IS NOT NULL))`（:44-47，注释 "mirror v1 0235"）；`pg.rs:99-116` claim_due 过滤 `status IN (0,1) AND available_at <= clock_timestamp() AND (lease_expires_at IS NULL OR lease_expires_at <= clock_timestamp())`；claim 不改写 available_at（只铸 lease_expires_at + claim_token + attempts+=1 :125-131）——「claimed 行 age > 86400 ⇒ lease 必已过期 ⇒ 活 relay 已重领」语义链成立（**prompt-claiming 前提**，见 §4 FM-10） |
| E9 | aero-eng 依赖/尺寸/测试面约束 | ✅ 全中：`checks.rs:256` ALLOWED_DEPS `("aero-eng", &["aero-common", "serde", "tokio"])`（检查只审计 `aero-*` workspace 内部依赖 :294-296，实际强制项 = aero-common——加 `aero-audit-connector` 即红，需改 checks.rs + dependency-check.sh = 出范围）；`audit_provision.rs` = **790 行**（file-size-check.sh MAX_LINES=800 比较符 `-gt`，**801 即 WARN**，余量 10 行）；`tests/audit_provision.rs` = **487 行**、:5 `use aero_eng::audit_provision::*;` glob import；`ConnParams` :375 pub、`parse_db_url` :385 pub（~56 行自包含）；`PsqlRunner` :437 **私有**、`query` :466 **私有**（新模块不能自建 runner，`run_relay` 必须调 `audit_provision::run`）；`lib.rs` 已 `pub use outcome::{Outcome, Severity}` :45 + `pub mod audit_provision` :50；`cli_smoke.rs`/`cli_integration.rs` 零 audit-provision-check/relay 引用（grep 无命中）→ main.rs 增量不触 cli_smoke |
| E10 | （补充）`run_priority` 先跑 `run()` 短路先例 + `run()` 报告/退出契约 | ✅ `run_priority` :665-672：base error 直接返回（`if base.is_error() { return base; }`）——`run_relay` 阶段 1 同构；`run()` :552 起，Q0-Q5 全走 `runner.query`（`-At` 单行输出 :466-485，trimmed；零行 = 空串），结尾 `print!("{report}")` + FailClosed→`Outcome::error` / Consistent|Healthy→`Outcome::ok` :646-656；`verdict()` 文案 `verdict: fail-closed — {reason}`（:209 harness grep 前缀） |

**结论**：requirements 全部证据成立；历史核对中的两处事实更正（E6 probe 缺失、E7 leg C 已存在）均已由当前实现复验，全部约束（E8 CHECK、E9 尺寸/依赖/可见性）通过。本设计补充实现级发现：**E9-PsrqlRunner 私有**（`run_relay` 的 base 阶段只能调 `audit_provision::run`，不能自建 runner）、**E10-短路先例**（run_priority 同构）、**§4 FM-10 假阳性窗口**（available_at 计龄代理在「积压 > 24h 后 relay 才启用」场景的瞬时误报，须显式文档化）。

## §2 API 变更

### 2.1 新模块 `crates/aero-eng/src/relay_runtime.rs`（目标 ≤250 行）

```rust
//! Relay runtime-health leg (B5-2): Q7 stuck-lease signal + the unified
//! `--relay` probe gate. Split out of audit_provision.rs (790/800 WARN
//! line) — same size-discipline precedent as the B5-1 sibling module.

use crate::outcome::Outcome;
use std::process::Stdio;

/// Lease upper bound, cloned from aero-audit-connector/src/relay.rs
/// (normative source; aero-eng cannot depend on the connector crate —
/// checks.rs ALLOWED_DEPS). Kept in lockstep by the Q7 stuck-lease test.
pub const MAX_LEASE_SECONDS: i64 = 86_400;

/// Q7 — oldest claimed-row age in seconds (B5-2 runtime-health leg): a
/// claimed row (status=1 with a fencing token) whose age from `available_at`
/// exceeds MAX_LEASE_SECONDS has certainly outlived its lease under prompt
/// claiming — `claim_due` reclaims expired-lease rows every tick (pg.rs), so
/// an old claimed row means no live relay is running. `available_at` is not
/// rewritten at claim time; the lease is minted at claim >= available_at and
/// clamped to <= MAX_LEASE_SECONDS (aero-audit-connector/src/relay.rs).
/// `{table}` is only ever one of the fixed [`G0239_CANDIDATES`] literals
/// (parse_probe_line-resolved) — no user input reaches the SQL.
pub const Q7_SQL: &str = "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 1 AND claim_token IS NOT NULL";

/// Parse the Q7 single-value line (psql `-At`; zero rows = empty string).
pub fn parse_claimed_age(out: &str) -> Result<Option<i64>, String> {
    if out.is_empty() {
        return Ok(None);
    }
    out.parse::<i64>()
        .map(Some)
        .map_err(|e| format!("invalid oldest-claimed age {out:?}: {e}"))
}

/// Report line, Q4-symmetric (`oldest-pending-age: Ns` / `n/a`).
pub fn claimed_age_line(secs: Option<i64>) -> String {
    match secs {
        Some(secs) => format!("audit-provision-check: oldest-claimed-age: {secs}s"),
        None => "audit-provision-check: oldest-claimed-age: n/a".to_string(),
    }
}

/// The B5-2 probe's nine named scenarios — the harness relay-mock-probe leg
/// greps `^probe: .*: PASS$` count == 9; names are the contract from
/// docs/design/2026-08-08-aero-cli-b5-2-relay-probe-bin-landing.design.md
/// §2.1 (probe PASS lines carry no trailing detail).
pub const RELAY_PROBE_SCENARIOS: [&str; 9] = [
    "happy_path", "forbidden_403", "permanent_422", "permanent_409",
    "receipt_mismatch", "transient_500", "transient_timeout",
    "lease_invariant", "fencing_stale_token",
];

/// Ok iff: child exit 0; every REQUIRED scenario has a line-anchored
/// `probe: <name>: PASS` (whitespace-tolerant, trailing detail rejected —
/// harness `^probe: .*: PASS$` anchors the line end); and no
/// `probe: …: FAIL` line is present. Unknown-name PASS lines are not
/// penalized (the probe may grow scenarios).
pub fn validate_probe_output(exit_ok: bool, stdout: &str) -> Result<(), String> {
    if !exit_ok {
        return Err("relay probe exited non-zero".to_string());
    }
    for name in RELAY_PROBE_SCENARIOS {
        let expected = format!("probe: {name}: PASS");
        if !stdout.lines().any(|l| l.trim_end() == expected) {
            return Err(format!("missing probe PASS line: {expected}"));
        }
    }
    if stdout.lines().any(|l| l.starts_with("probe: ") && l.contains(": FAIL")) {
        return Err("relay probe reported a FAIL line".to_string());
    }
    Ok(())
}

/// `--relay` mode: base check (fail-closed priority before probe) → spawn
/// the B5-2 probe with captured output → validate the named PASS lines.
pub async fn run_relay(db_url: &str) -> Outcome {
    // Stage 1 — base check first: dead / relay-off+undelivered / stuck-lease
    // fail before the probe spawns (AC4; run_priority precedent).
    let base = crate::audit_provision::run(db_url).await;
    if base.is_error() {
        return base;
    }
    // Stage 2 — probe spawn (legacy network relay-probe arm contract, the
    // sole difference = piped output we echo back after the child exits).
    let mut cmd = if let Ok(bin) = std::env::var("AERO_RELAY_PROBE_BIN") {
        tokio::process::Command::new(bin)
    } else {
        let mut c = tokio::process::Command::new("cargo");
        c.args(["run", "--quiet", "-p", "aero-audit-connector", "--bin", "aero-audit-relay-probe"]);
        c
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::error(format!("cannot launch relay probe: {e}")),
    };
    // Take the pipes BEFORE wait: the timeout branch must be able to
    // kill+reap the child (dropping a `wait_with_output` future mid-flight
    // would orphan a hung probe holding the cargo lock). Probe output is
    // ~9 short lines — far under the 64KB pipe buffer, so reading the
    // pipes after wait cannot deadlock; a hung child still trips the 120s
    // timeout, is killed, and its buffered partial output is then readable.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let status = match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait(),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Outcome::error(format!("relay probe wait failed: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Outcome::error("relay probe timed out after 120s");
        }
    };
    let mut out = String::new();
    let mut err = String::new();
    if let Some(p) = out_pipe.as_mut() {
        let mut buf = Vec::new();
        if tokio::io::AsyncReadExt::read_to_end(p, &mut buf).await.is_ok() {
            out = String::from_utf8_lossy(&buf).into_owned();
        }
    }
    if let Some(p) = err_pipe.as_mut() {
        let mut buf = Vec::new();
        if tokio::io::AsyncReadExt::read_to_end(p, &mut buf).await.is_ok() {
            err = String::from_utf8_lossy(&buf).into_owned();
        }
    }
    print!("{out}");
    eprint!("{err}");
    // Stage 3 — exit-code mapping (stricter than the legacy arm: exit 2 is
    // an error here — `--relay` is a verdict gate, not a probe launcher).
    let exit_ok = match status.code() {
        Some(0) => true,
        Some(1) => return Outcome::error("relay probe: one or more scenarios FAILED"),
        Some(2) => return Outcome::error("relay probe usage error — cannot verify B5-2 state machine"),
        _ => return Outcome::error(format!("relay probe exited abnormally: {status}")),
    };
    match validate_probe_output(exit_ok, &out) {
        Ok(()) => {
            println!("audit-provision-check: relay-probe: PASS (9/9)");
            Outcome::ok("")
        }
        Err(reason) => {
            println!("audit-provision-check: relay-probe: FAIL — {reason}");
            Outcome::error(format!("relay probe: {reason}"))
        }
    }
}

// --- Relocated from audit_provision.rs (pub use re-export, §5) ---
// ConnParams + parse_db_url (verbatim move; the struct is pub and the
// parser is harness-compatible: password travels via PGPASSWORD, never argv).
```

### 2.2 `audit_provision.rs` 增量（净变化 −20 行 → ≤800）

1. **搬迁**：`ConnParams` :375 + `parse_db_url` :385（~56 行含 doc）移出 → 顶部 `pub use crate::relay_runtime::{ConnParams, parse_db_url};`（glob import 测试透明，E9）。
2. **`G0239Counts`**（:92-105）增字段：
   ```rust
   /// Oldest claimed-row age in seconds (None = no claimed rows). Q7.
   pub oldest_claimed_secs: Option<i64>,
   ```
3. **`run()`** `Some(table)` 分支内、Q4 之后（:599-610 附近）：
   ```rust
   let q7 = match runner.query(&Q7_SQL.replace("{table}", table)).await {
       Ok(o) => o,
       Err(e) => return outcome_error(e),
   };
   let oldest_claimed_secs = match parse_claimed_age(&q7) {
       Ok(s) => s,
       Err(e) => return outcome_error(e),
   };
   ```
   构造点补 `oldest_claimed_secs,`（非整数输出 → exit 1，与 Q4 同契约；表缺席 → 不跑不印行，`not migrated` 行覆盖）。
4. **`format_report`**（:170-192）`Some(g)` 分支 `oldest-pending-age` 行之后追加 `claimed_age_line(g.oldest_claimed_secs)` 输出（`use crate::relay_runtime::claimed_age_line;`——truth-check 零调用守卫由 format_report 调用满足）。
5. **`verdict()`**（:134-155）既有两支**逐字节不动**，在 relay-off 检查之后、`Verdict::Healthy` 之前追加（`use crate::relay_runtime::MAX_LEASE_SECONDS;`）：
   ```rust
   // Q7 stuck lease: a claimed row older than the lease bound means the
   // relay died after claiming (a live relay reclaims expired-lease rows
   // each tick — pg.rs claim_due filter). Fail-closed; never folded into
   // pending backlog. Reachable only when relay enabled (relay-off with
   // claimed rows already fails in branch 2 — claimed counts as
   // undelivered; relay-off with zero claimed rows yields None → no-op).
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
   - 阈值**严格大于**（== 86400 不 fail，防时钟抖动误报）。
   - 文案前缀 `verdict: fail-closed —` 不变（harness grep 不破）。
   - **base `run()` 即生效**（Q7 是快照输入，非 `--relay` 专属）；`run_priority` 经 `run()` 自动继承。

### 2.3 `aero-cli/src/main.rs` 臂增量（纯增量，~3 行）

```rust
Some("--relay") => aero_eng::relay_runtime::run_relay(&url).await,
// usage 文案改为:
Some(_) => Outcome::error("usage: audit-provision-check [--priority|--relay]"),
// c! 宏 help/description 追加:
//   ; --relay = B5-2 relay state-machine probe (spawns aero-audit-relay-probe;
//   Healthy requires all 9 probe PASS lines)
```

`--priority` 臂、`network relay-probe` 臂、`cmds` 串零改动。

### 2.4 `lib.rs` + tests 机械面

- `lib.rs` 增 `pub mod relay_runtime;`（:50 块内）。
- `tests/audit_provision.rs` 7 个 `G0239Counts` 构造点（:96/:119/:152/:175/:204/:242/:298）机械补 `oldest_claimed_secs: None`（或按用例语义设值）。

## §3 兼容性约束

| 面 | 约束 | 保证机制 |
|---|---|---|
| `network relay-probe` 臂 | 零改动（AC5 no contract break） | 本设计不动 :428-481；probe 落地前后行为不变（缺 bin → exit 1 同现状，sibling 设计 :27 实跑取证 `exit status: 101`） |
| `--priority` 面 | 零改动，且**自动继承 Q7** | `run_priority` 先跑 `run()`（E10）——Q7 行 + stuck-lease 支随 base 报告进入 priority 面 |
| `verdict()` 既有两支 | dead-first → relay-off+undelivered **逐字节保留** | §2.2-5 纯追加支，位于两检查之后；单测 ②③ 钉优先级（R9） |
| 既有 harness leg B/C/D | 语义与 grep 零改动 | 新腿插在 leg D 后、leg C 前；leg C 的 `WHERE status = 0 LIMIT 1` 不触 E 的 status=1 行；leg E 行保持 status=1 → C 的 `dead=1`/`delivered=0` 断言不受影响（E 行 claimed ≠ dead ≠ delivered） |
| 48/48 pin / slot 清单 | 零新 slot、b5-pin.sh 零改动 | `b5_check` append（E7）+ pin 只要求 `^B5-CHECK ${entry}: (PASS|SKIP)( \|$)` ≥1 行——leg E/F/F′ 全复用 `audit-provision-check` slot |
| 依赖图 | 零新依赖 | `MAX_LEASE_SECONDS` clone 字面量 + 锁步测试（§5/§7）；checks.rs / dependency-check.sh 零改动 |
| 迁移 | 零 SQL 迁移 | 0239 CHECK 已含 claim-state 对约束（E8）；Q7 只读 |
| 报告行兼容 | 新 `oldest-claimed-age:` 与既有 `oldest-pending-age:` 并存 | 完整前缀 grep 纪律（§8）；测试断言均 `.contains` 子串（:222/:232 等）——新行不含 `oldest-pending-age` 子串（claimed ≠ pending），`report_not_migrated_has_no_age_or_dead_lines`（:227）在 `g0239: None` 时无新行（claimed_age_line 只在 Some(g) 分支调用） |
| cli_smoke / cli_integration | 零改动 | 二者不引用本命令（E9） |
| 尺寸 | `audit_provision.rs` 不得新增 WARN（801 即 WARN，`-gt`） | 790 − 56（搬迁）+ ~35（增量）≈ 769 ≤ 800；`relay_runtime.rs` ≤ 250；tests ≤ 800（487 + ~140） |
| 历史 probe 缺席窗口（phase-1） | `--relay` fail-closed（spawn error），harness leg F file-gate SKIP | 当前 probe 已落地并走 PASS；file-gate 作为防御性守卫保留；leg E/F′ 无 file-gate，缺 bin 仍 fail-closed |

## §4 失败模式（FM）

| # | 场景 | 行为 | 理由/恢复 |
|---|---|---|---|
| FM-1 | Q7 psql 查询错误（DB 断/表结构异常） | `outcome_error` → exit 1 | 与 Q0-Q5 同契约：门一旦跑 Q7 就不静默跳过 |
| FM-2 | Q7 输出非整数 | `parse_claimed_age` Err → exit 1 | 与 Q4 解析同契约 |
| FM-3 | probe spawn 失败（bin 缺席 → cargo 101；`AERO_RELAY_PROBE_BIN` 指向不存在路径） | `cannot launch relay probe: {e}` → exit 1（fail-closed） | 门无法验证 B5-2 即不 Healthy；harness leg F file-gate 保证 phase-1 窗口绿 |
| FM-4 | probe 超时 120s | `child.kill()` + `child.wait()`（reap）+ `relay probe timed out after 120s` → exit 1；缓冲的部分输出照常回显 | 与 legacy 臂同款 kill-on-timeout（E3）。**设计要点**：pipe 在 wait 前 take——绝不用 `wait_with_output` 包 timeout（future 被 drop 时 child 成孤儿、cargo lock 悬挂，harness 后续腿会撞锁）；probe 输出 ~9 短行 ≪ 64KB pipe 缓冲，wait 后读 pipe 无死锁风险 |
| FM-5 | probe exit 1（任一场景 FAIL） | `one or more scenarios FAILED` → exit 1 | 臂契约映射 |
| FM-6 | probe exit 2（usage） | **error**（比 legacy 臂的 `warning(2)` 更严） | `--relay` 是 verdict 门，非 probe 启动器——usage 意味着无法验证 |
| FM-7 | probe exit 0 但零/缺 PASS 行（如 stub bin `/bin/true`） | 校验器 Err（首个缺失名）→ `relay-probe: FAIL — missing probe PASS line: probe: lease_invariant: PASS` → exit 1 | 不信任 exit code 单点（AC2 的 stub-bin 注入面 = harness leg F′） |
| FM-8 | PASS 行带尾随 detail / 任一 `probe: …: FAIL` 行 | 校验器 Err → exit 1 | E6 契约：`^probe: <name>: PASS$` 锚定行尾；belt-and-braces 防 probe 未来加场景并 FAIL |
| FM-9 | base fail（dead / relay-off+undelivered / stuck-lease） | 阶段 1 短路，**probe 不 spawn** | AC4 原句：dead 仍压一切；probe 绝不先于 DB-fail 状态 |
| FM-10 | **假阳性窗口**（文档化限制）：积压 > 24h 后 relay 才启用 → 活 relay 首轮 claim 的行的 `available_at` 年龄 > 86400 但 lease 全新 | stuck-lease fail-closed（瞬时） | 语义链前提是 **prompt-claiming**（E8 引文原文）；relay 按 `available_at` 升序 claim，随行 settle（status→2）窗口自动关闭（健康 sink 下分钟级）。方向安全（fail-closed 而非误放行）；改进项（改以 `lease_expires_at` 计龄）需动 acceptance 文本，标为 out-of-scope 后续 |
| FM-11 | 时钟边界 | `secs > MAX_LEASE_SECONDS` 严格大于；== 86400 不 fail | 防时钟抖动误伤（保守） |
| FM-12 | probe 缺席 + `--relay` 手工跑 | FM-3 路径 exit 1（fail-closed） | harness leg F 显式 SKIP，两 direction 并行不互阻塞 |
| FM-13 | leg F′ 的 `/bin/true`（exit 0、零输出） | 校验器 Err → exit 1 + `relay-probe: FAIL` | 证明不信任 exit code 单点（AC2） |
| FM-14 | stderr 捕获回显 | probe 构建噪音/panic 原样到 stderr | 失败时可诊断；PASS 行在 stdout 不受影响 |

## §5 迁移步骤

**零 SQL 迁移**（0239/0240/0241 不动；Q7 只读、CHECK 已含 claim-state 对约束 E8）。代码面迁移三步：

1. **模块搬迁**：`ConnParams` + `parse_db_url`（~56 行）从 `audit_provision.rs` 原样移入 `relay_runtime.rs`；`audit_provision.rs` 顶部 `pub use crate::relay_runtime::{ConnParams, parse_db_url};`——公开 API 不变，`tests/audit_provision.rs` glob import 透明解析（E9）。`PsqlRunner`（私有）+ `query`（私有）**留在原位**（E9/E10：run_relay 不得自建 runner，base 阶段调 `audit_provision::run`）。
2. **功能接线**：§2.2 的 5 处增量（字段 / Q7 查询 / 报告行 / verdict 支 / 构造点）+ §2.3 臂增量 + §2.4 lib.rs。**先 `cargo build`**（本仓迁移纪律 §4.2 同款：任何编译期嵌入变更后先 build 再验证）。
3. **测试/harness 面**：tests 构造点机械补字段 + 新用例（§7）；`test-integration.sh` T-11 块 leg D 与 leg C 之间插入 F′ → F → E（§7 顺序纪律）；`b5-pin.sh` 零改动。

种子 SQL（leg E，E8 CHECK 三列齐设、单时钟域）：
```sql
INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload, available_at, claim_token, lease_expires_at)
VALUES (gen_random_uuid(), 1, 'message', 10,
        jsonb_build_object('event_id', gen_random_uuid()::text, 'source_system', 'aero-im.source'),
        clock_timestamp() - interval '2 days', gen_random_uuid(), clock_timestamp() - interval '1 hour');
```

## §6 实现预算

| 文件 | 现状 | 增量 | 目标 | 阈值 |
|---|---|---|---|---|
| `relay_runtime.rs` | 新建 | ~230 行（Q7_SQL/parse/line/常量/场景/校验器 ~90 + run_relay ~85 + 搬迁 ~56） | ≤250 | 800 WARN（file-size-check.sh） |
| `audit_provision.rs` | 790 | −56 搬迁 + ~35 增量（字段 4 + Q7 块 9 + 报告行 4 + verdict 支 12 + use 3 + 构造点 1） | ≈769 | **≤800（-gt，801 即 WARN）** |
| `tests/audit_provision.rs` | 487 | ~+140（7 构造点机械补 + 解析器 3 例 + 报告 3 例 + verdict 6 例 + 校验器 7 例 + 锁步 2 例） | ≈627 | ≤800 |
| `main.rs` | — | +3（match 臂 + usage + help） | — | — |
| `lib.rs` | — | +1（`pub mod relay_runtime;`） | — | — |
| `test-integration.sh` | — | +~45（F′ ~12 + F ~14 + E ~15 + 注释） | — | — |

## §7 测试验收映射（AC 原句 → 可测断言）

| AC（requirements §5 原句） | 可测断言 | 位置 |
|---|---|---|
| **AC1** Q7 oldest claimed-row age + 报告行 + > MAX_LEASE_SECONDS 翻 fail-closed + 种子 claimed 行验证 | 单测：`parse_claimed_age("42")→Some(42)` / 空串→`None` / `"abc"`→`Err`；`claimed_age_line(Some(42))`==`audit-provision-check: oldest-claimed-age: 42s`、`None`→`…n/a`；verdict ① `Some(90_000)`→FailClosed 含 `stuck lease`、④ `Some(86_400)`→Healthy、⑤ `Some(1)`→Healthy、⑥ `None`→Healthy；harness leg E：种子 SQL（2 天前 available_at + 1 小时前过期 lease）→ base 模式 exit 非零 + `grep -E 'audit-provision-check: oldest-claimed-age: [0-9]{6,}s'` + `verdict: fail-closed` + `stuck lease` | R1/R2/R3/R8.3/R9 |
| **AC2** `--relay`：base + spawn（AERO_RELAY_PROBE_BIN）+ captured-output，全 PASS 行在场才 Healthy | 单测：`validate_probe_output` 9/9→Ok、`exit_ok=false`→Err、缺 `lease_invariant`→Err 指明缺失名、PASS 行带尾随 detail→Err、9/9 + 一条 `probe: x: FAIL boom`→Err、空输出→Err；leg F′：`AERO_RELAY_PROBE_BIN=/bin/true` → exit 非零 + `relay-probe: FAIL`；leg F（file-gate）：实 probe → exit 0 + `relay-probe: PASS (9/9)` + `grep -c '^probe: .*: PASS$' == 9`；缺席 → 显式 SKIP 文案不 FAIL | R5/R6/R8.1/R8.2/R9 |
| **AC3** Healthy = relay on + bindings>0 + 无 dead + 无 stuck lease + 全部 9 probe PASS | leg F 全链正例（leg-D 态：relay on + binding + pending 积压 + probe 9/9 → exit 0）；leg E/F′ 各负例（stuck / stub 各 exit 1）；verdict 单测 ①③④⑤⑥ 钉阈值与优先级 | R3/R5/R8/R9 |
| **AC4** fail-closed 优先级不变：dead 压一切；relay-disabled+undelivered 在任何 probe 之前 fail | verdict 单测 ② `dead=1`+stuck 同场 → dead 理由（dead-first）；③ relay off + undelivered + stuck 同场 → `no audit:event:write grant issued`；`run_relay` 阶段 1 error 短路（单测层面由 base 函数契约 + leg E 证明：stuck 时 probe 不 spawn——leg E 输出无 `relay-probe:` 行） | R3/R5/R9 |
| **AC5** 扩展 tests + test-integration.sh leg（throwaway 库）；legacy `network relay-probe` 无 contract break | leg E/F/F′ 复用 T-11 块既有 `create_throwaway_database`/migrate/`drop_created_database`（AGENTS §4.3）；leg C 语义/grep 零改动（E7）；`network relay-probe` 臂零改动（E3）；`cargo test --workspace --lib` + `cargo clippy --workspace --all-targets`（无新警告）+ `scripts/truth-check.sh`（新函数有调用、零 allowlist 新增）+ `scripts/file-size-check.sh`（audit_provision.rs 无新 WARN）+ `cargo test -p aero-audit-connector --all-targets`（零 connector 改动，基线不回归） | R7/R8/R9/R10 |

**harness 顺序纪律**（T-11 块内）：leg D（既有，relay on healthy）→ **leg F′**（stub 负向，无 file-gate）→ **leg F**（实 probe 正例，file-gate）→ **leg E**（stuck-lease 种子 + base 负例）→ **leg C**（既有，dead-row 负例）。F′/F 在 leg-D 健康态运行（base 不短路，probe 必 spawn）；E 在 F 之后（E 的 status=1 行不扰 F 的 9/9 断言——F 只 grep PASS 行）；C 最后（其 `WHERE status = 0 LIMIT 1` 不触 E 行）。

## §8 协调与硬规则（AGENTS §4）

- **probe bin 边界**：`aero-audit-relay-probe.rs` 归 sibling direction（aero-cli-b5-2-relay-probe-bin-landing）——本设计只消费其钉死契约（9 场景名、`probe: <name>: PASS` 行格式、退出码 0/1/2）；`--relay` 在 probe 缺席时 fail-closed（FM-3），leg F file-gate 显式 SKIP，并行不互阻塞。
- **零新 slot / 零 b5-pin 改动**：leg E/F/F′ 全复用 `audit-provision-check` slot（b5_check append + pin ≥1 行，E7）；48/48 保持。
- **零新依赖 / 零迁移**：`MAX_LEASE_SECONDS` clone + 锁步测试；checks.rs / dependency-check.sh / migrations 零改动。
- **fail-closed 顺序**：dead → relay-off+undelivered → stuck-lease → probe；任一 fail 即短路，probe 绝不先于 DB-fail 状态 spawn（FM-9）。
- **种子 CHECK 纪律**：status=1 种子必须 claim_token + lease_expires_at 齐设（E8）；`available_at`/`lease_expires_at` 用 `clock_timestamp()` 系（单时钟域）。
- **grep 精确性**：新行 `audit-provision-check: oldest-claimed-age:` 与既有 `oldest-pending-age:` 并存（完整前缀）；`relay-probe: PASS (9/9)` 与 probe 自身 `probe: <name>: PASS` 并存（后者以 `^probe: ` 锚定）。
- **尺寸纪律**：audit_provision.rs 保持 ≤800（§5 搬迁）；新代码落 relay_runtime.rs；tests ≤800。
- **编译期嵌入纪律**：迁移/搬迁后先 `cargo build` 再跑 harness（AGENTS §4.2）。
- **活验证**：leg E/F/F′ 内建 throwaway 库纪律（T-11 块既有 create/migrate/drop）。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/truth-check.sh`（新函数有调用，零 allowlist 新增）· `scripts/{file-size-check,web-check}.sh` · `cargo test -p aero-audit-connector --all-targets`（既有基线不回归）。
