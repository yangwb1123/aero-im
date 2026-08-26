# Requirements Spec — aero-ai：governance outbox 健康度 Prometheus gauges（status 0/1/2/3 桶 + oldest-pending 年龄 + dead-row 告警，闭环 relay 反馈回路）

- **Module (analysis root)**: `crates/aero-ai/src`（`metrics.rs` = gauge emit 层模式锚；新交付面 = `audit_outbox_health.rs` + `lib.rs` re-export 链；跨 crate 消费面 = `aero-common::metrics::names`（gauge 名词汇表）、`aero-eng::audit_provision`（Q3/Q4 SQL 源）、`crates/aero-server/src/bin/boot/metrics_tasks.rs`（采样 timer，AGENTS.md §2 `observability_gauge_samplers` 模式）、`crates/aero-server/tests/`（交叉核对测试）、`scripts/b5-pin.sh`（gauge pin））
- **Direction**: "Operationalize governance-outbox health: Prometheus gauges for status 0/1/2/3 buckets + oldest-pending age + dead-row alarm (completing the relay feedback loop)"（value 6 / risk_reduction 7 / effort 3 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`；gate anchor `docs/campaigns/implementation-gate.md`（G6 当前为 "48/48（27 个可执行 slot + 21 个 [PROPOSED]）、T-11、moderation 优先级"）
- **Sibling specs（同批次，文件面协调）**: `2026-08-08-aero-eng-relay-runtime-health-leg.req.md`（T-11/relay 腿与 aero-eng 模块纪律——**本 spec 复用其 E9 依赖审计结论**）、`2026-08-09-aero-ai-moderation-finalize-contract-pin.req.md`（同模块 governance.rs 面）、`2026-08-08-aero-cli-b5-4-audit-provision-check.req.md`（CLI 本体，本 spec 零改动）
- **Status**: Requirements（下述证据全部经源码实读核对，核对日期 2026-08-09；行号为核对时锚点、会漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-09
- **Current pin/ownership note (2026-08-26)**: live `B5_CONTRACT_TEST_LIST` 为 48 个 slot（27 个非 `[PROPOSED]` 可执行 slot + 21 个 `[PROPOSED]` 仓外占位）；“executed”是 manifest 分类，不是本文件声称已经运行的测试数。`audit_outbox_health` 仍保留只读查询与注入式 library/test emitter；server B5-4 Tier-1/Tier-2 status-label sampler 是唯一 runtime owner，负责 exact `aero_audit_outbox_oldest_pending_secs` series。

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs` Q3/Q4/Q5（buckets / oldest-pending age / dead-row detail——"reusable verbatim as gauge queries"） | ✅ **Verified**。`Q3_SQL` :49 = `SELECT status, count(*) FROM {table} GROUP BY status ORDER BY status`；`Q4_SQL` :52 = `SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 0`；`Q5_SQL` :55 = dead 详情 ≤5 行。`G0239_CANDIDATES` :27 = `["audit_governance_outbox", "audit_outbox"]`。**Q3/Q4/Q5 均为私有 `const`**——gauge 采样器复用需改 `pub`（visibility-only，零语义变化）。`parse_buckets` :304 / `parse_probe_line` :292 / `parse_db_url` :389 已 pub。文件现 **788 行**（800 WARN / 1200 HARD，`scripts/file-size-check.sh`）——**本 spec 对 aero-eng 零新增行**（sibling E9 纪律：不得新增 WARN） |
| E2 | `crates/aero-audit-connector/src/pg.rs:98-130`（claim_due；dead-terminal 语义） | ✅ **Verified**。`claim_due` 现 :101-131：`status IN (0,1)` + `available_at <= clock_timestamp()` + lease 过期过滤，`ORDER BY priority DESC, available_at, created_at, event_id` + `FOR UPDATE SKIP LOCKED`——**只 SELECT/UPDATE 自己认领的行**，gauge 采样器（纯 SELECT）与其零交互。R3 混合优先级 claim 测试 `mixed_priority_claim_orders_moderation_first_then_fifo` 在 :530-624（`#[ignore = "requires live Postgres (DATABASE_URL)"]`，40 backlog priority 10 + 10 admin priority 100，limit 25 断言 claimed set）——**回归门** |
| E3 | `relay.rs:39-48 is_dead_at`、`:196 403 immediate dead` | ✅ **Verified**。`PERMANENT_DEAD_AT = 2` + `pub fn is_dead_at(attempts) -> bool`（permanent 类 attempt≥2 即 dead，≤1 次重试）；`deliver_claim` Forbidden 臂（~:196）注释明言 "Deliberately NOT `is_dead_at`: HTTP 403 is fail-closed immediate death (T-11)" → `mark_dead` 直接落 status=3。**status=3 = 终态**（direction 的 "dead-terminal" 语义链成立） |
| E4 | `migrations/0241_governance_reconcile.sql`（status=3 永不复活，恢复手动） | ✅ **Verified**。SQL 注释明言 "dead rows are NEVER resurrected: status=3 rows exist in the outbox, so NOT EXISTS skips them — the reconciler cannot fight the relay's terminal state (**403-outage recovery stays manual, per design**)"。→ 正是 dead 告警的动机：无自动恢复，必须持续可见 |
| E5 | Pattern anchor：AGENTS.md §2 `observability_gauge_samplers`（三独立 timer，query Err 留旧值） | ✅ **Verified**。`crates/aero-server/src/bin/boot/metrics_tasks.rs`：DB pool+WHIP 15s（:22-52）、AI DLQ 30s（:128-167）、NATS backlog 30s（:168-191）——统一形态 = `tokio::time::interval(secs)` + `MissedTickBehavior::Skip` + `ai_shutdown` cancel token + `select!` 循环 + `Ok → common_metrics::set_gauge(_labeled)` / `Err → tracing::warn!`（**不 set，旧值保留**）。全程无 DB 写。采样器经 `boot::spawn_metrics_tasks` 在 main.rs 装配（`bin/main.rs` ~:268-276） |
| E6 | aero-ai metrics.rs gauges（emit 层模式） | ✅ **Verified**。`crates/aero-ai/src/metrics.rs`（637 行）：gauge 名常量在 **`aero_common::metrics::names`**（`AI_QUEUE_DEPTH` :96、`AI_DEAD_LETTER_QUEUE_SIZE` :119 等平台词汇表），aero-ai 是 **emit 层**（`set_queue_depth` 等）；每 recorder 收 `&Registry`（注入式），单测用 `Registry::new()` + `render_prometheus()` 断言（**fresh registry，绝不碰 process-global**）；`aero_` 命名空间 + snake_case。aero-ai `Cargo.toml` 已有 sqlx/tokio/tracing/aero-common 生产依赖——**新模块零新增依赖** |
| E7 | `[PROPOSED]`：boot/aero-audit-connector 无 audit_governance_outbox gauge sampler | ✅ **Verified（gap 成立）**。`rg audit_governance|audit_outbox` 于 boot/ 零命中（metrics_tasks.rs 无 audit 面）；aero-audit-connector 全 crate **零 metrics/Registry 引用**；`aero_common::metrics::names` 无 audit 系列。outbox 健康目前只经 `audit-provision-check` CLI（psql 手动/CI 腿）与 drill 可见——**无连续信号，direction 问题陈述精确** |
| E8 | `scripts/b5-pin.sh:44` pin list | ✅ **Verified（含一处事实修正，见 §1.1 ②）**。:44 = `audit-provision-check`（B5_CONTRACT_TEST_LIST 最后执行槽）；当前 live pin 为 48 槽（27 个可执行 slot + 21 个 `[PROPOSED]`）；`assert_b5_contract_pin` **硬性 `count -ne 48` FAIL**——"extend pin list with the gauge names" 若直接加槽会击穿 48/48 守卫，须以**独立 `B5_GAUGE_PIN` 列表**落家（同文件、同守卫风格、48 槽计数不变）。`test-b5-pin-guard.sh` 为纯 bash 守卫测试（无 DB），`test-integration.sh` :753-756 在 gate closure 调用守卫 |
| E9 | T-11 drill（closed token endpoint；pending=N, dead=0） | ✅ **Verified**。`crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`：loopback 端口 bind+drop = 确定性 closed token endpoint（无 wall-clock 窗口）；每 round 断言 `pending == total`（N 1:1 + window + spill）、`COUNT(status IN (1,2,3)) == 0`（:195/:214——**never falsely dead**）、`SUM(attempts) == total*round`（非 vacuous）、`last_error LIKE '%audit connector HTTP transport failed%'`（closed endpoint 真被尝试）。harness 腿 `scripts/test-integration.sh` ~:380-447（T-11 throwaway 库 → migrate → drill → audit-provision-check legs → drop，`b5_check "t11-fail-closed"`） |
| E10（支撑） | 0239 表 / `OutboxStatus` / /metrics 端点 | ✅ `migrations/0239_audit_governance_outbox.sql`：`status INTEGER CHECK (status IN (0,1,2,3))`；`aero-common/src/model/audit.rs` `OutboxStatus { Enqueued=0, Claimed=1, Delivered=2, Dead=3 }` + `from_i32`（fail-open 扫描语义）；`routes/routes.rs:547-548` `/metrics` bearer 门控 `metrics_handler` |

### 1.1 对 direction 陈述的钉化 / 事实修正（evidence-backed）

- **钉化① 查询/emit library 与 runtime owner 分离**：`audit_outbox_health` 保留只读查询、注入式 library/test emitter 与 dead-alarm 单测；server B5-4 Tier-1/Tier-2 status-label sampler 是 runtime 唯一 owner，boot 不再调用 `set_audit_outbox_gauges`，避免覆盖 `aero_audit_outbox_oldest_pending_secs`。Q3/Q4 SQL 文本以 **verbatim 副本 + 文本级 cross-pin 测试**（aero-ai 常量 == aero-eng 常量，字符串相等）实现 "reusable verbatim"——本仓既有 leaf↔DDL cross-pin 先例（0239 注释、`ddl_contract_defaults_and_checks`）。
- **修正② "cross-checked in an aero-eng test" 不可行——依赖 lint 硬约束**：`crates/aero-eng/src/checks.rs` `ALLOWED_DEPS` :256 `("aero-eng", &["aero-common", "serde", "tokio"])`，且 `parse_deps` :414-445 **同时扫描 `[dev-dependencies]`**（三个 section 均解析）——aero-eng 测试 crate 加 `aero-ai`/`sqlx` dev-dep 即触发 `check_deps` 违规（改 ALLOWED_DEPS 超出本 direction，sibling E9 同结论）。**交叉核对测试落家 = `crates/aero-server/tests/audit_outbox_gauges.rs`**（`ROOT_CRATE = "aero-server"` "may depend on everything"，:264；aero-server 已依赖 aero-eng :30 + aero-ai :35 + sqlx；`tests/authz_lint.rs` 为既有 root-crate 测试先例）。核对参照侧仍是 **aero-eng 的 Q3/Q4 常量**（文本 pin + 同 DB 值核对），acceptance 原句在 §5 保留并 re-ground。
- **修正③ "extend scripts/b5-pin.sh:44 pin list with the gauge names"**：直接加槽 → `assert_b5_contract_pin` `-ne 48` FAIL（E8）。落家 = b5-pin.sh 新增 **`B5_GAUGE_PIN`** 数组（5 个全名）+ `assert_b5_gauge_pin` 守卫（5 条、无重复、`^[a-z0-9_]+$`），`B5_CONTRACT_TEST_LIST` 48 槽计数**不变**；harness gate closure 追加守卫调用 + 源码 grep（pin ↔ Rust 常量漂移守卫，`assert_no_production_stub_references` 先例）。
- **钉化④ "15-30s tick"**：固定 **15s**（区间内；与 DB-pool 采样器同拍），`MissedTickBehavior::Skip` + cancel token，无 env（与三核心采样器一致，AGENTS.md §2）。"dead>0 for >1 poll interval" = **连续两个采样 tick 均 dead>0** → 纯函数 `dead_alarm(prev_dead, cur_dead) = prev>0 && cur>0` 触发 `warn!`（tracing，不写 DB）。
- **钉化⑤ 采样器"never writes"**：`sample_outbox_health` 只含 SELECT 语句（to_regclass 探测 + Q3 + Q4）；read-only 由 aero-server 测试的**行状态快照 before/after 相等**断言机械钉住（status/attempts/last_error/delivered_at 逐字段），与 relay 的 claim/settle/mark_dead 零交互。

## 2. Verified current state（缺口盘点）

```
outbox 健康可观测面（全部 verified）：
  CLI 手动  ： aero-cli audit-provision-check（Q0..Q5 + verdict，psql-backed）——仅 operator 手动 / CI 腿
  drill     ： t11 / priority / relay / l1-parity（一次性断言，非连续信号）
  连续信号  ： ✅ server B5-4 已接入 audit outbox Tier-1 30s + Tier-2 可配置只读采样器；status-label 契约为 runtime 唯一 owner
  SQL 可复用 ： ✅ Q3/Q4 在 aero_eng::audit_provision 公开并由 server plain pin 交叉核对
  依赖约束  ： aero-eng ALLOWED_DEPS 禁 aero-ai/sqlx（含 dev-deps）→ 交叉核对测试须落 root crate
  pin 面    ： b5-pin.sh 48 槽守卫硬性 -ne 48；5 个 gauge 名仍由独立 B5_GAUGE_PIN 列表钉住
```

**Gap this direction closes**（all verified）：① 5 个 Prometheus gauge（`aero_audit_outbox_{pending,claimed,delivered,dead,oldest_pending_secs}`），由 library helper 与 server B5-4 status-label runtime sampler 对齐 Q3/Q4；② dead>0 持续 >1 poll interval → `warn!`（终态无自动恢复，0241 语义）；③ 所有采样路径纯读，与 relay/claim/moderation 优先级零交互。runtime exact oldest-pending series 仅由 server B5-4 Tier-2 写入；方向外不扩：不做 Q5 dead-detail gauge、不做自动恢复、不改 CLI 语义、不加 B5 槽。

## 3. Scope

**In scope（effort 3 的完整切片）**：
- `crates/aero-common/src/metrics.rs`：`names` 模块新增 5 个 gauge 名常量（平台词汇表，`AI_QUEUE_DEPTH` 同款）。
- `crates/aero-ai/src/audit_outbox_health.rs`（**新模块**，本 direction 核心交付）：`Q3_SQL`/`Q4_SQL` verbatim 副本 + `AuditOutboxCounts` + `sample_outbox_health(pool)`（纯 SELECT）+ `set_audit_outbox_gauges(reg, &counts)` + `dead_alarm(prev, cur)` + plain 单测（fresh Registry，E6 模式）。
- `crates/aero-ai/src/lib.rs`：`pub mod audit_outbox_health;` + re-export 链（:36-40 追加，`aero_ai::audit_outbox_health::*` 与 `aero_ai::*` 均可 grep）。
- `crates/aero-eng/src/audit_provision.rs`：`G0239_CANDIDATES`/`Q3_SQL`/`Q4_SQL` → `pub`（**visibility-only，零新增行**，788 行守住 800 WARN）。
- `crates/aero-server/src/bin/boot/metrics_tasks.rs`：B5-4 Tier-1 30s + Tier-2 可配置 sampler（interval + Skip + cancel + 成功写入 / 错误 warn 留旧值）；status-label sampler 为唯一 runtime owner。
- `crates/aero-server/tests/audit_outbox_gauges.rs`（**新**）：文本 cross-pin（plain）+ 值交叉核对（DB-gated `#[ignore]`）+ read-only 证明 + T-11 posture 场景。
- `scripts/b5-pin.sh` + `scripts/test-b5-pin-guard.sh` + `scripts/test-integration.sh`：`B5_GAUGE_PIN` + 守卫 + harness 接线（48 槽不变；27 个可执行 slot + 21 个 [PROPOSED]）。

**Out of scope**（direction 验收未点名）：
- Q5 dead-detail（`event_id,last_error`）gauge——acceptance 只要 5 个 gauge；Q5 保持 aero-eng 私有，CLI 独享。
- 任何 DB 写 / 自动恢复（0241 "recovery stays manual" 语义不动；采样器永不写）。
- relay / claim / settle / mark_dead / reconcile 的任何改动；`priority`/`class` claim 排序（B5-3 handoff 面）。
- v1 `snaplink_delivery_outbox` gauge（独立 relay 路径，Q1 不采样）。
- `audit-provision-check` CLI 本体与 verdict 语义零改动（sibling 面）；drill 零改动。
- 新 B5 契约槽（48/48 计数不变）；Tier-2 使用既有 `AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 配置。
- aero-eng ALLOWED_DEPS / checks.rs / dependency-check.sh 改动（依赖 lint 不动，测试落家已 re-ground，§1.1 ②）。
- 同分析文件 direction #1（L1 aggregation，`governance.rs` [PROPOSED] 面）与 #2（grant-time fail-closed gate）——另行 spec。

## 4. Requirements

### R1 — gauge 名词汇表（aero-common leaf）：5 个平台 gauge 名常量

`crates/aero-common/src/metrics.rs` `names` 模块新增（`AI_QUEUE_DEPTH` :96 同款文档注释风格，`aero_` 前缀 + snake_case）：

```rust
// --- Audit governance outbox (B5-1 relay feedback loop) ---
/// Gauge: audit_governance_outbox rows in status 0 (enqueued/pending).
pub const AUDIT_OUTBOX_PENDING: &str = "aero_audit_outbox_pending";
/// Gauge: rows in status 1 (claimed, lease held).
pub const AUDIT_OUTBOX_CLAIMED: &str = "aero_audit_outbox_claimed";
/// Gauge: rows in status 2 (delivered).
pub const AUDIT_OUTBOX_DELIVERED: &str = "aero_audit_outbox_delivered";
/// Gauge: rows in status 3 (dead — terminal, manual recovery only, 0241).
pub const AUDIT_OUTBOX_DEAD: &str = "aero_audit_outbox_dead";
/// Gauge: age in seconds of the oldest status-0 row (0 when none pending).
pub const AUDIT_OUTBOX_OLDEST_PENDING_SECS: &str = "aero_audit_outbox_oldest_pending_secs";
```

**Acceptance**：
- A1.1 五个常量存在且值恰为上述字面量（全名带 `aero_` 前缀——`scripts/b5-pin.sh` `B5_GAUGE_PIN` 与 `/metrics` 暴露共用同一拼写）。
- A1.2 常量进入 `names` 模块（`aero_common::metrics::names::AUDIT_OUTBOX_*` 路径可 grep）；aero-common 零新依赖（leaf 不变式）。
- A1.3 `AUDIT_OUTBOX_OLDEST_PENDING_SECS` 的 help 语义 = "oldest status-0 row age"（与 Q4 的 `WHERE status = 0` 一致，非所有行）。

### R2 — aero-ai 新模块 `audit_outbox_health`：采样器 + emit 层 + dead 告警纯函数（本 direction 核心增量）

`crates/aero-ai/src/audit_outbox_health.rs`（新文件，~200 行；`aero-ai` 已有 sqlx/tokio/tracing/aero-common 生产依赖，**零新依赖**）：

```rust
/// Q3/Q4 的 verbatim 副本（源 = aero_eng::audit_provision::{Q3_SQL,Q4_SQL}）。
/// 文本相等由 aero-server tests 的 cross-pin 断言机械钉住（§5 A2.1）——本仓
/// leaf↔DDL cross-pin 先例；改任一侧必须同步另一侧，否则测试红。
pub const Q3_SQL: &str = "SELECT status, count(*) FROM {table} GROUP BY status ORDER BY status";
pub const Q4_SQL: &str = "SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM {table} WHERE status = 0";

/// 单次采样的 outbox 健康快照（镜像 aero_eng::audit_provision::G0239Counts 的
/// 5 个数值字段；dead 是独立终态桶，永不折入 delivered）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditOutboxCounts {
    pub pending: i64,              // status 0
    pub claimed: i64,              // status 1
    pub delivered: i64,            // status 2
    pub dead: i64,                 // status 3（终态，0241 手动恢复）
    pub oldest_pending_secs: Option<i64>, // Q4；None = 无 pending 行
}

/// 纯 SELECT 采样：to_regclass('audit_governance_outbox') 探测 → 缺失 ⇒ Ok(None)
///（调用方跳过本 tick，旧 gauge 值保留——E5 模式）；存在 ⇒ Q3/Q4 执行 + 解析。
/// 永不写库、永不 claim/settle/mark_dead/reconcile。
pub async fn sample_outbox_health(pool: &sqlx::PgPool) -> Result<Option<AuditOutboxCounts>, sqlx::Error>

/// emit 层：把一次快照写入注入式 Registry 的 5 个 gauge（E6 模式；用
/// aero_common::metrics::names::AUDIT_OUTBOX_*，生产接 global()）。
pub fn set_audit_outbox_gauges(reg: &Registry, counts: &AuditOutboxCounts)

/// dead 告警纯函数："dead>0 持续 >1 poll interval" = 上一 tick 与当前 tick 均
/// dead>0（prev>0 && cur>0）。纯 + 总，无 I/O，boot timer 每 tick 调用。
pub fn dead_alarm(prev_dead: i64, cur_dead: i64) -> bool
```

`crates/aero-ai/src/lib.rs`：`pub mod audit_outbox_health;` + re-export（`pub use audit_outbox_health::{AuditOutboxCounts, Q3_SQL, Q4_SQL, dead_alarm, sample_outbox_health, set_audit_outbox_gauges};`——:36-40 链追加）。

**Acceptance**：
- A2.1 `sample_outbox_health` 只含 SELECT（代码评审 + A5.3 read-only 测试双钉）；表缺失 → `Ok(None)`（不 panic、不 error——pre-0239 boot 窗口静默跳过，同 relay 的 degrade 类）。
- A2.2 `AuditOutboxCounts` 恰含 5 字段；`oldest_pending_secs` 语义 = Q4 空结果 → `None`（`query_optional`，RowNotFound 不升 error）。
- A2.3 `set_audit_outbox_gauges` 收 `&Registry`（注入式）；单测用 `Registry::new()` + `render_prometheus()` 断言 5 个全名 + 值（fresh registry，E6 模式；`i64 as f64` 带 `#[allow(clippy::cast_precision_loss)]` + 注释，同 boot 先例）。
- A2.4 `dead_alarm` 真值矩阵：`(0,0)→false`、`(0,3)→false`（首见不告警）、`(3,0)→false`（已恢复）、`(3,3)→true`（持续 >1 interval）——4 例 plain 单测。
- A2.5 新模块所有符号 `aero_ai::audit_outbox_health::*` 与 `aero_ai::*` 双路径可 grep（lib.rs re-export 链）。

### R3 — aero-eng SQL 常量 pub（verbatim 复用的源侧，零语义变化）

`crates/aero-eng/src/audit_provision.rs`：`G0239_CANDIDATES`（:27）、`Q3_SQL`（:49）、`Q4_SQL`（:52）改 `pub const`。**不新增任何行**（788 行守住 800 WARN，sibling E9 纪律）。

**Acceptance**：
- A3.1 三常量 pub；`audit-provision-check` CLI 行为逐字节不变（同文本同执行路径）。
- A3.2 aero-eng 既有纯单测（`tests/audit_provision.rs`，487 行，verdict 矩阵 / parse 系）零改动保持绿。

### R4 — server B5-4 status-label samplers（runtime 唯一 owner）

`crates/aero-server/src/bin/boot/metrics_tasks.rs` 的 B5-4 sampler 是
`audit_governance_outbox` gauges 的唯一 runtime writer：Tier-1 每 30s 写入
`aero_audit_outbox_status{status=...}` 的 enqueued/claimed 与 dead signal，
Tier-2 按 `AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 写入 delivered/dead
及 `aero_audit_outbox_oldest_pending_secs`。`aero_ai::audit_outbox_health`
仍提供只读 SQL、注入式 library/test emitter 和 dead-alarm 单测，但 boot
不调用 `set_audit_outbox_gauges`，避免第二个 runtime writer 覆盖 exact
oldest-pending series。

两层 sampler 均使用 `MissedTickBehavior::Skip`、共享 cancel token；成功才
写入对应快照，表缺失/查询错误只记录 warning 并保留 last-good values。
所有查询路径均只读，不调用 relay/claim/reconcile。

**Acceptance**：
- A4.1 Tier-1 30s、Tier-2 可配置，均 Skip + cancel；status-label sampler
  是唯一 runtime owner。
- A4.2 全路径零 DB 写（无 `UPDATE`/`DELETE`/`INSERT`/`TRUNCATE` 调用）；
  不调用 relay/claim/reconcile 任何面。
- A4.3 查询成功才更新；查询 Err 保留 prior gauges，Tier-1 同时将
  `sampler_up` 置 0 并递增错误 counter。
- A4.4 `aero_ai::audit_outbox_health` 的 library/test emitter 仍可在注入
  registry 中验证五个系列；这不构成 server runtime writer。

### R5 — 交叉核对 + read-only + T-11 posture 测试（root-crate，`crates/aero-server/tests/audit_outbox_gauges.rs`）

新测试文件（`tests/authz_lint.rs` 先例；aero-server 已依赖 aero-eng/aero-ai/sqlx，零新依赖；tokio workspace "full" 含 macros+rt-multi-thread，`#[tokio::test]` 可用）：

- **R5.1（plain，无 DB）文本 cross-pin**：`assert_eq!(aero_ai::audit_outbox_health::Q3_SQL, aero_eng::audit_provision::Q3_SQL)` + Q4 同款——verbatim 复用机械钉住；另断言 5 个 `aero_common::metrics::names::AUDIT_OUTBOX_*` == 字面量全名（pin↔Rust 常量双钉）。
- **R5.2（`#[ignore = "requires live Postgres (DATABASE_URL)"]`，throwaway 库）值交叉核对**：self-isolating `TRUNCATE`（pg.rs 先例）→ seed 覆盖 0/1/2/3 四态 + 已知 `available_at` 年龄的 pending 行（status=1 行须同时设 `claim_token` + `lease_expires_at`——0239 CHECK，sibling E8 种子约束）→ `sample_outbox_health` 结果 == 同 DB 上执行 Q3/Q4 文本（aero-eng 常量，sqlx）的结果 == `set_audit_outbox_gauges` 渲染进 fresh `Registry` 的 5 个 gauge 值；psql 可用时（`AERO_PSQL_MODE` 双模）追加 psql `-At` 输出相等腿。
- **R5.3（同 DB-gated）read-only 证明**：采样前后对全部行做 (status, attempts, claim_token, lease_expires_at, last_error, delivered_at, available_at) 快照，逐字段相等——采样器永不写。
- **R5.4（同 DB-gated）T-11 posture**：seed 仅 status=0 行 + `attempts` 递增（复刻 closed-token-endpoint drill 的终态：`COUNT(status IN (1,2,3)) == 0`）→ `sample_outbox_health` = `{pending: N, claimed: 0, delivered: 0, dead: 0}`——**never falsely dead**。

**Acceptance**：
- A5.1 R5.1 两个 plain 测试在 `cargo test -p aero-server --test audit_outbox_gauges`（无 env）真实执行绿。
- A5.2 R5.2 在 harness throwaway 已迁移库上 PASS（值 == Q3/Q4 同 DB 输出）；seed 常量以 `AERO_AUDIT_DRILL_ROWS` 式局部常量控制，方程不依赖具体 N。
- A5.3 R5.3/R5.4 绿——"sampler never writes" 与 "never falsely dead" 有机械断言，非仅评审。
- A5.4 测试文件自隔离（TRUNCATE-at-start，pg.rs 先例），共享库并发安全（`--jobs 1` 序列化，E8 主套件注释）。

### R6 — b5-pin gauge pin（48/48 gate 的 live 面，48 槽计数不变）

`scripts/b5-pin.sh`：新增（B5_CONTRACT_TEST_LIST 旁）：

```bash
# B5 gauge pin — audit governance outbox health gauges (requirements:
# 2026-08-09-aero-ai-governance-outbox-health-gauges.req.md R1). Full
# Prometheus names, `aero_` prefix; guard = exactly 5, no dupes, [a-z0-9_].
B5_GAUGE_PIN=(
    aero_audit_outbox_pending
    aero_audit_outbox_claimed
    aero_audit_outbox_delivered
    aero_audit_outbox_dead
    aero_audit_outbox_oldest_pending_secs
)
```

+ `assert_b5_gauge_pin`（`${#B5_GAUGE_PIN[@]} -eq 5`、无重复、`^[a-z0-9_]+$`）；`scripts/test-b5-pin-guard.sh` 增正/负例（计数≠5 / 重复 / 畸形名 → FAIL）；`scripts/test-integration.sh` gate closure（~:753 `assert_b5_contract_pin` 旁）调用守卫 + **源码 grep**：每个 pin 名须出现在 `crates/aero-common/src/metrics.rs`（`assert_no_production_stub_references` 先例）——bash pin ↔ Rust 常量漂移双钉。

**Acceptance**：
- A6.1 `B5_GAUGE_PIN` 恰 5 条全名（与 `AUDIT_OUTBOX_*` 常量拼写逐字符一致）；`assert_b5_gauge_pin` 纯 bash 可独立运行。
- A6.2 `B5_CONTRACT_TEST_LIST` 当前恰 48 槽（`assert_b5_contract_pin` `-ne 48` 不触发；27 个可执行 slot + 21 个 `[PROPOSED]`）——**新增守卫不改变 48/48 契约**；`test-b5-pin-guard.sh` 全例绿（含新增 gauge 例）。
- A6.3 harness 在 gate closure 执行 `assert_b5_gauge_pin` + 源码 grep；任一 pin 名与 Rust 常量漂移 → harness 红（fail-closed，非 warn）。
- A6.4 harness 新增 R5 测试执行腿（T-11 throwaway 库 drop 前：`DATABASE_URL="$T11_DRILL_URL" cargo test -p aero-server --test audit_outbox_gauges -- --ignored --test-threads=1`），PASS 则复用 `b5_check "audit-provision-check" "PASS"`（**同 slot 多行合法**——sibling E7 先例；不新增槽）；0239 未落地 → 随既有腿 SKIP。

### R7 — 回归门（零行为改动）

**Acceptance**：
- A7.1 `mixed_priority_claim_orders_moderation_first_then_fifo`（pg.rs R3，DB-gated）零改动、harness `--ignored` 腿保持 PASS——gauge 纯读不触碰 claim 排序（R4 不调用 claim 面）。
- A7.2 `aero-audit-t11-drill.rs` 零改动、`t11-fail-closed` 腿 PASS（A6.4 腿与 drill 共享库但自隔离 TRUNCATE 先行——pg.rs 先例）。
- A7.3 `audit-provision-check` CLI 零改动、leg D/C 语义不动（A3.1 文本逐字节同）。
- A7.4 `cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（零新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规（`audit_outbox_health.rs` ~200 行 < 800 WARN；audit_provision.rs 788 行零新增）。

## 5. Acceptance → testable mapping（direction 原句保留并 re-ground）

| Direction acceptance（原文） | Testable artifact | Location | Status |
|---|---|---|---|
| "New sampler (15-30s tick) exposes audit_outbox_{pending,claimed,delivered,dead} + audit_outbox_oldest_pending_secs as Prometheus gauges; values equal the Q3/Q4 psql output on the same DB (cross-checked in an aero-eng test)" | R1（5 常量）+ R2（`sample_outbox_health`/`set_audit_outbox_gauges`）+ R4（server B5-4 Tier-1/Tier-2 sampler）+ R5.1/R5.2（文本 pin + 同 DB 值核对，**Q3/Q4 参照侧 = aero-eng 常量**；psql 腿可用时追加）。re-ground：**"in an aero-eng test" → `crates/aero-server/tests/audit_outbox_gauges.rs`**（aero-eng `ALLOWED_DEPS` 禁 aero-ai/sqlx 含 dev-deps，§1.1 ②；参照侧仍为 aero-eng 的 Q3/Q4 SQL） | aero-common/src/metrics.rs（names）+ aero-ai/src/audit_outbox_health.rs + boot/metrics_tasks.rs + aero-server/tests/audit_outbox_gauges.rs | ✅ 已实现并通过 plain/DB-gated 交叉验证 |
| "48/48: extend scripts/b5-pin.sh with the gauge-name pin; the server B5-4 sampler owns runtime writes and retains last-good values on errors" | R6（`B5_GAUGE_PIN` + `assert_b5_gauge_pin` + guard 测试 + 源码 grep；48 槽计数不变）+ R2 library `dead_alarm` 矩阵 + R4 owner/last-good contract + R5.3 read-only 证明 | scripts/{b5-pin,test-b5-pin-guard,test-integration}.sh + aero-ai/src/audit_outbox_health.rs + aero-server/src/bin/boot/metrics_tasks.rs + aero-server/tests/audit_outbox_gauges.rs | ✅ 已实现；live DB legs 仍以环境为准 |
| "T-11: run aero-audit-t11-drill.rs against a relay with a closed token endpoint while sampler runs — gauges must show pending=N, dead=0 (never falsely dead) throughout the drill" | R5.4（复刻 drill 终态：全 status=0 + attempts>0 → pending=N, dead=0）+ R7.2（drill 本体零改动 PASS）+ R4.2（采样器无写路径，不可能把 pending 翻转成 dead） | aero-server/tests/audit_outbox_gauges.rs（T-11 posture 场景） | ✅ 已实现并通过 ignored DB-gated 验证 |
| "Moderation priority unaffected: gauge is read-only; existing pg.rs R3 mixed-priority claim test stays green" | R7.1（R3 零改动 PASS）+ R4.2/R5.3（采样器 SELECT-only，不触碰 claim/reconcile） | pg.rs 既有测试（回归）+ audit_outbox_health.rs（只读构造） | ✅ 既有（回归验证） |

## 6. Harness gates & coordination

- **执行腿**：R5 测试 = aero-server 集成测试文件（`cargo test -p aero-server --test audit_outbox_gauges`；plain 两测随常规 `--lib`? **不**——integration test 文件不进 `--lib` 主套件，须 A6.4 专用腿跑 `-- --ignored`）。R5.1 plain 腿在 harness 无 DB 阶段即可跑（或随 T-11 腿一并）。
- **48/48 不变**：`B5_CONTRACT_TEST_LIST` 48 槽零改动（27 个可执行 slot + 21 个 `[PROPOSED]`）；gauge 名以 `B5_GAUGE_PIN` 独立列表落家（§1.1 ③）。新腿复用 `audit-provision-check` slot 的 verdict 行（多行合法，sibling E7）。
- **cross-pin 分工**：aero-server plain 文本 pin（R5.1）→ aero-ai Q3/Q4 ↔ aero-eng Q3/Q4 漂移早段失败；DB-gated 值核对（R5.2）→ 全链（SQL→parse→counts→gauges）值相等；psql 腿 → CLI 传输面交叉。
- **Runtime owner note**：AGENTS.md §2 的历史 15s audit-outbox 描述不再是当前 owner；server B5-4 Tier-1 30s + Tier-2 可配置 status-label sampler 负责 runtime 写入，`audit_outbox_health` 只保留 library/test emitter。
- **提交前必过**（AGENTS.md §4.3）：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` 零新警告 · `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规（aero-eng 788 行零新增行；`audit_outbox_health.rs` 新文件 < 800；aero-server tests 文件不在 lib 尺寸面）。

## 7. Out of scope（明确不交付）

- Q5 dead-detail gauge（`event_id,last_error`）——acceptance 5 gauge 之外；Q5 保持 aero-eng 私有。
- 任何自动恢复 / DB 写（0241 "recovery stays manual" 语义原地不动；采样器永不写）。
- relay/claim/settle/mark_dead/reconcile 改动；priority/class claim 排序（B5-3 handoff 面）。
- v1 `snaplink_delivery_outbox` gauge（Q1 不采样，独立 relay 路径）。
- `audit-provision-check` CLI 本体/verdict/leg D/C 语义改动；drill 本体改动。
- 新 B5 契约槽 / 48 计数改动；除既有 Tier-2 `AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 外不新增 env 配置。
- aero-eng `ALLOWED_DEPS`/checks.rs/dependency-check.sh 改动（依赖 lint 不动；测试落家 re-ground，§1.1 ②）。
- 同分析文件 direction #1（L1 aggregation，`governance.rs` [PROPOSED] 面）与 #2（grant-time fail-closed gate）——各自另行 spec。
