# Design — aero-eng B5-1: in-tx coverage/consistency probe（audit_events ↔ outbox 1:1 覆盖探针）

- **落家**: `crates/aero-eng`（`audit_coverage.rs` 新兄弟模块 + `audit_provision.rs` 薄接线 + harness 腿 C + 单测 + 37/37 换槽）
- **Requirements**: `docs/requirements/2026-08-08-aero-eng-b5-1-in-tx-coverage-probe.req.md`
- **宿主命令**: `audit-provision-check`（`crates/aero-cli/src/main.rs:489-506` 命令臂，**本设计零改动**；`--priority` 经 `run_priority → run()` 自动继承）
- **Sibling 设计**: `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`（命令本体）、`docs/design/2026-08-08-aero-eng-b5-1-operation-class-coverage-leg.design.md`（同模块同批次 direction #1）、`docs/design/2026-08-07-aero-cli-b5-acceptance-gate-harness.design.md`（leg 纪律）
- **Status**: Design（证据核验全部完成，见 §1；实现预算 §2.3 精确到行；两处实现级更正 D1/D8 见 §2.1/§4.5；**Rev 2（本 review）**：Q6–Q10 收进单 `-c` 显式 `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY` 单快照——D9 见 §2.1、实证 E11 见 §1、F16 见 §4；**Rev 3（本 review）**：F18 运营决定——target-DB 判定 + 悬崖量化 + 缓解 D10/旋钮/假红包络与 rerun 政策 + F19 sweep 交互确认，见 §4.1）

## §1 证据核验（untrusted claims → 源码逐条对照，行号仅核对时锚点）

| # | 声称 | 核验结果 |
|---|---|---|
| E1 | 0239：`event_id UUID PRIMARY KEY`（= audit_events.id 1:1）+ trigger `aero_enqueue_governance_audit`（Gate 1 fail-open / token-keyed `message.moderated`-only / Gate 2 binding RAISE P0001 / `ON CONFLICT (event_id) DO NOTHING`） | ✅ 全中。`:29` `event_id UUID PRIMARY KEY`（注释钉死 "UNIQUE(event_id)" dedup 契约）；`:76-78` Gate 1（enabled 关 → `RETURN NEW`）；`:83-84` token-keyed（`NEW.action <> 'message.moderated'` → RETURN NEW）；`:103-107` Gate 2 `binding := aero_snaplink_binding_for_workspace(NEW.workspace_id)`；0235 函数体 `RAISE EXCEPTION 'commercial binding is unavailable' USING ERRCODE = 'P0001'` 在 **:206-207**（spec 引 193-205，同一函数，行号漂移）；`:131` `ON CONFLICT (event_id) DO NOTHING`。trigger `audit_events_governance_enqueue`（`:136-139`，AFTER INSERT FOR EACH ROW） |
| E2 | 0236 v1 路径全 action 入 `snaplink_delivery_outbox`，`UNIQUE(destination, idempotency_key)` | ✅ `aero_enqueue_snaplink_audit` :68 + trigger `audit_events_snaplink_delivery` :130-131（AFTER INSERT）；0235 DDL 的 `UNIQUE(destination, idempotency_key)` 与同事务双 trigger 并存确认——**腿 C 重复种子必须先删 v1 行，否则 v1 UNIQUE 先 abort**（§4.5 D8） |
| E3 | `audit_provision.rs` 790 行、`PsqlRunner` :437、`run()` :552、`run_priority()` :665、`verdict()`、解析器先例、`G0239_CANDIDATES` | ✅ 全中（`wc -l` = 790；`:27` `G0239_CANDIDATES`、`:106` `AuditSnapshot`、`:134` `verdict`、`:437` `struct PsqlRunner`、`:552` `run`、`:665` `run_priority`、`:225/256/300/324/353` 解析器）。**实现级新增发现：`PsqlRunner` 与其 `query` 方法均私有**（:437/:466）——新模块的 `probe` 无法执行 SQL，须 `pub(crate)` 化（§2.1 D1，sibling 设计 E10 同款备选路径） |
| E4 | `aero-storage/src/audit_governance.rs` 五个 db_test（parity in-tx oracle / A2 half 4 / duplicate / unmapped / reconcile）全 `#[ignore]` PG 门控 | ✅ 全中：`:251` `moderation_finalize_outbox_parity`、`:773` `moderation_finalize_runtime_disabled_commits_1_plus_0`（**A2 half 4：关窗 |G|=0**）、`:818` `non_moderation_action_passes_through_unmapped`、`:883` `duplicate_event_id_is_deduped_by_on_conflict`（**复合 PK 容许同 id 重复 + ON CONFLICT 吞第二发**）、`:964` `governance_reconcile_backfills_disabled_window`；fixture `enable_enforcement_with_binding` :120-160 的 proven 列集 = 腿 C 种子照抄源 |
| E5 | harness leg B 位置 + `AUDIT_PROVISION_DB` + helpers | ✅ `test-integration.sh`：leg B 注释 :250、B1 :264-281、B2 :282-301、drop :302、`b5_check "audit-provision-check" "PASS"` :303、else-SKIP :305；`:38` `AUDIT_PROVISION_DB="aero_audit_provision_$$"`、`:40` `assert_disposable_db_name`（:49-62 条目清单）、`:93` `run_psql`、`:101` `create_throwaway_database`、`:117` `drop_created_database`；同 slot 多腿先例（T-11 块内 legs D/C 于 :385/:440 三次 b5_check）；0239 文件门先例 :306-324（SKIP-with-reason :324-325/:445-446） |
| E6 | 0146 分区表，复合 PK `(id, created_at)`，`id` 不唯一 | ✅ `:93` `PRIMARY KEY (id, created_at)`、`:94` `PARTITION BY RANGE (created_at)`、`:99-100` `audit_events_default` 兜底分区——orphan 公式落 audit 侧的 DDL 依据成立 |
| E7 | 0241 回填 + 留存硬删（`AERO__SERVER__AUDIT_RETENTION_DAYS` 默认 365）+ connector 每 claim batch 调 reconcile | ✅ 0241 `aero_reconcile_governance_audit`：enabled-binding join :67-68、`action = 'message.moderated'` :69、NOT EXISTS :70、INSERT 与 0239 逐字节相同 :88；`connector/pg.rs:81-91` `reconcile()` 调 `aero_reconcile_governance_audit`；`retention.rs:24` env 读取 `.unwrap_or(365)`；`audit.rs` `sweep_before` 硬删 + legal-hold 豁免（`:55-71` 起） |
| E8 | 37/37 pin + guard 自测硬编码串 + 词表 | ✅ `b5-pin.sh`：`B5_CONTRACT_TEST_LIST` 实测 **37 = 15 executed + 22 [PROPOSED]**（python 精确计数）；`assert_b5_contract_pin` :83 起（`count -ne 37` :86-88、slot 正则、vacuous、证据行）；`contract-test-22[PROPOSED]` :67；`test-b5-pin-guard.sh` **:64 与 :113** 硬编码 `"B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS"` 确认；`aero-common/model/audit.rs`：`OutboxStatus` :20、`AuditClass`（枚举序 Message→Room→Admin，`const fn as_str()` :133-141 产出 `message/room/admin`）、`MODERATION_OUTBOUND_ACTION="admin.content.flag"` :150、`LOCAL_ACTION_MODERATED="message.moderated"` :156、`GOVERNANCE_CLASS_*` :164-168；truth-check AUDIT-FLAG mirror `truth-check-lib.sh:278`（rg `crates --glob '*.rs'`，`admin.content.flag` 仅 audit.rs:150 + allowlist） |
| E9 | （补充核对）`audit_events` trigger 集合 | ✅ 全仓 grep：`audit_events` 上仅两个 AFTER INSERT trigger（0236 `audit_events_snaplink_delivery` + 0239 `audit_events_governance_enqueue`，'g' < 's'）——Gate 2 RAISE 先于 v1 副作用，原子性腿的 abort 证据成立 |
| E10 | （补充核对）依赖面 | ✅ `aero-eng/Cargo.toml`：aero-common / tokio / serde_json / anyhow / async-trait / time / serde——**零新依赖**；新解析器纯 `split('|')` 即可（Q9 的 class 列被 0239 CHECK 锁死 `admin|message|room`，无换行/`|` 注入面，**不需要** sibling direction #1 的 json_agg framing——那是 action 自由文本列的防御） |
| E11 | （本轮 review 实证）`PsqlRunner::query` 每次调用 = 独立 psql 会话/独立快照；且**多语句单 `-c` 在 READ COMMITTED 下仍逐语句快照**（psql 隐式事务不改隔离级）；显式 `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY` 单 `-c` 才给单快照 | ✅ 本机 aero-postgres throwaway 库实证（已 drop）：`SELECT count(*)`; `pg_sleep(3)`; `SELECT count(*)`（1.5s 并发插入）→ `1`/`2`（无显式隔离，逐语句快照）vs `2`/`2`（显式 RR RO，单快照）；mid-string `1/0` + ON_ERROR_STOP → exit 1（fail-closed 保留）；`-q` 可压 `BEGIN`/`COMMIT` tag 行（路由层跳过亦可——选后者，`PsqlRunner` 零改动）；Q9 空结果 → 该节零行（聚合语句恒产一行，tag 路由可区分） |

**结论**：requirements 全部证据成立；三处事实更正（orphan 公式落 audit 侧 / enabled-window+binding-scope 豁免 / 留存窗口收窄）复验通过。本设计在其上补两个实现级决定：**D1**（`PsqlRunner`+`query` 改 `pub(crate)`，全部 Q6-Q10 执行收进 `audit_coverage::probe`，`audit_provision.rs` delta ≤13 行）与 **D8**（腿 C 重复种子必须先删 v1 行；`if run_psql; then 腿 FAIL; fi` 断言 abort 路径）。

## §2 API 变更

### 2.1 新模块 `crates/aero-eng/src/audit_coverage.rs`（目标 ≤190 行）

```rust
//! B5-1 in-tx coverage/consistency probe (Q6-Q10): the 1:1
//! audit_events ↔ audit_governance_outbox contract on a deployed DB —
//! missing/duplicate/orphan event_ids + per-class counts. SELECT-only,
//! non-destructive; split out of audit_provision.rs (790/800) — same
//! size-discipline precedent as the tests file header.

use aero_common::model::audit::AuditClass;

/// Q6 — governance-lane 1:1 coverage: message.moderated audit rows in
/// ENABLED-binding workspaces missing their outbox row. The enabled-binding
/// join is the 0241 reconciler's "mapped subset"; the runtime-disabled
/// window (A2 half 4) is exempted in the verdict, not here.
/// 每条 SELECT 首列 `'Qx' AS probe_tag`——五语句收进单 `-c`（D9）后按首 cell 路由，缺节/超行可 fail-closed。
pub(crate) const Q6_SQL: &str = "SELECT 'Q6' AS probe_tag, COUNT(*) \
FROM audit_events a \
LEFT JOIN {table} g ON g.event_id = a.id \
LEFT JOIN snaplink_commercial_bindings b \
       ON b.workspace_id = a.workspace_id AND b.enabled \
WHERE a.action = 'message.moderated' \
  AND g.event_id IS NULL \
  AND b.workspace_id IS NOT NULL";

/// Q7 — 1:1 dedup/ambiguity (the acceptance's COUNT != COUNT(DISTINCT),
/// on the coverage join's AUDIT side): audit_events.id is NOT unique
/// (0146 composite PK (id, created_at)); the outbox ON CONFLICT silently
/// swallows the second fire. Aggregate → always exactly one row (`f|0`
/// on an empty table).
pub(crate) const Q7_SQL: &str = "SELECT 'Q7' AS probe_tag, (COUNT(a.id) <> COUNT(DISTINCT a.id))::text, COUNT(a.id) \
FROM audit_events a \
WHERE a.action = 'message.moderated'";

/// Q9 — outbox per-class lane counts; class names print via
/// AuditClass::as_str() (aero-common leaf, audit.rs:164-168) — no bare
/// literals in Rust.
pub(crate) const Q9_SQL: &str = "SELECT 'Q9' AS probe_tag, class, COUNT(*) FROM {table} GROUP BY class ORDER BY class";

/// Q10 — outbox total (1:1 side of the coverage line).
pub(crate) const Q10_SQL: &str = "SELECT 'Q10' AS probe_tag, COUNT(*)::bigint FROM {table}";

/// Q8 — orphan outbox rows (no source audit row), scoped to the audit-
/// retention window. `days` is a numeric literal built from
/// [`audit_retention_window_days`] — no parameter channel, no user input.
/// 0 = retention disabled = unbounded window (clause omitted).
/// clock_timestamp() matches the table DEFAULTs' single clock domain.
pub(crate) fn q8_orphan_sql(table: &str, days: i64) -> String {
    let mut sql = format!(
        "SELECT 'Q8' AS probe_tag, COUNT(*) FROM {table} g \
LEFT JOIN audit_events a ON a.id = g.event_id \
WHERE a.id IS NULL"
    );
    if days > 0 {
        sql.push_str(&format!(" AND g.created_at >= clock_timestamp() - INTERVAL '{days} days'"));
    }
    sql
}

/// Per-class counts, filled via AuditClass::as_str() lookups (missing → 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassCounts {
    pub admin: i64,
    pub message: i64,
    pub room: i64,
}

/// One coverage snapshot (0239 table present only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageCounts {
    pub audit_total: i64,
    pub outbox_total: i64,
    pub missing: i64,
    pub orphans: i64,
    pub dedup_violation: bool,
    pub classes: ClassCounts,
}

/// Q7 two-cell line `t|2` → (dedup_violation, audit_total). Missing cell /
/// garbage → Err (fail-closed). Reuses parse_psql_bool_line semantics.
pub fn parse_dedup_line(line: &str) -> Result<(bool, i64), String>;

/// Q9 lines `class|count` → observed pairs. Empty lines skipped, bad lines
/// ignored, unknown classes ignored (parse_buckets fail-open philosophy);
/// ON_ERROR_STOP has already failed the query.
pub fn parse_class_counts(out: &str) -> Vec<(String, i64)>;

/// Audit-retention window: AERO__SERVER__AUDIT_RETENTION_DAYS (double
/// underscore, figment convention; boot/retention.rs:24 same read),
/// default 365; unset/invalid → 365; negative → 0 (unbounded — the
/// conservative fail-closed direction). CLAMP to ≤365_000 (1000 years):
/// PG `INTERVAL '{days} days'` errors past int32 days and the timestamp
/// subtraction overflows past ~2.45M days (live-verified, §9.5 F17) — an
/// extreme-but-parseable env value must not fail the Q8 query.
pub fn audit_retention_window_days() -> i64;

/// Breach components, dead-priority order: dedup → orphans → missing
/// (relay-on only). The single derivation both [`coverage_verdict`] and
/// [`format_lines`] consume — the report third line and the verdict cannot
/// diverge (spec R5 isomorphism, test-pinned).
fn breach_components(c: &CoverageCounts, relay_enabled: bool) -> Vec<&'static str>;

/// None = no breach (or designed fail-open window). dedup and retention-
/// window orphans fail in EVERY runtime state; missing fails only when the
/// relay is enabled (A2 half 4: |G|=0 while disabled is the pinned design
/// state; 0241 reconciles on re-enable).
pub fn coverage_verdict(c: &CoverageCounts, relay_enabled: bool) -> Option<String>;

/// The three greppable `audit-provision-check:` coverage lines (third line
/// strictly isomorphic to the verdict matrix).
pub fn format_lines(c: &CoverageCounts, relay_enabled: bool) -> String;

/// Execute Q6-Q10 via the runner (called from run()'s Some(table) arm).
/// Any query/parse failure → Err → run() maps to Outcome::error (exit 1).
pub(crate) async fn probe(runner: &PsqlRunner, table: &str)
    -> Result<CoverageCounts, String>
{
    let sql = probe_snapshot_sql(table, audit_retention_window_days());
    parse_probe_output(&runner.query(&sql).await?)
}

/// Single-snapshot wrapper: all five statements inside one explicit
/// `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY … COMMIT`, sent as
/// ONE `PsqlRunner::query` call (one psql -c = one session = one snapshot).
/// Read-only + MVCC: takes no locks, never blocks the relay path.
pub(crate) fn probe_snapshot_sql(table: &str, days: i64) -> String;

/// Route `-At` output by first-cell tag (`Q6|`…`Q10|`): skip blank and
/// `BEGIN`/`COMMIT` lines, strip the tag, delegate to the section parsers
/// (`parse_dedup_line` / `parse_class_counts` contracts unchanged — the
/// tag lives only in the routing layer). Q6/Q7/Q8/Q10 are aggregates →
/// each must yield exactly one line (missing section or >1 line → Err,
/// fail-closed); Q9 may yield zero (empty outbox → all-zero classes).
/// Any other untagged non-empty line → Err.
pub fn parse_probe_output(out: &str) -> Result<CoverageCounts, String>;
```

**D9 — 单快照修正（本轮 review；实证 E11）**：原设计 Q6–Q10 各自独立 psql `-c` = 五个独立会话/五个快照——活库上语句间提交会让 missing/total/outbox 计数互相矛盾（如 Q6 后 binding re-enable + 0241 回填 → `missing=1` 假红；Q6 后 audit-only 行提交 → Q7 计入而 Q10 未计入、`missing=0` 假绿），报告描述的是一张从未存在的瞬间拼图，1:1 判定失去意义。修正：五语句收进**单次** `query()` 调用并显式包裹 `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY`。**「单 psql 调用」本身不够**：多语句 `-c` 在 READ COMMITTED 下仍逐语句快照（E11：`1`→`2`）；显式 RR RO 才给单快照（E11：`2`/`2`）。`PsqlRunner` 零额外改动（路由层跳过 `BEGIN`/`COMMIT`/空行，不需要 `-q`）；fail-closed 保留（语句中途错误 + ON_ERROR_STOP → 整调用 exit 1，实测）。

**D1 — 执行收口（实现级更正）**：`PsqlRunner`（`audit_provision.rs:437`）与其 `query` 方法（`:466`）均私有，`probe` 无法跨模块执行 SQL。两处 `pub(crate)` 化（sibling design §2.4 备选路径同款），随后 **Q6-Q10 的查询+解析+错误映射整体收进 `probe`**。`audit_provision.rs` 的 delta 因此只有 13 行（§2.3），不留 30 行内联探针块。`q8_orphan_sql` 独立为纯函数（单测覆盖 0/N 两窗）。

**D10 — 超时臂修正 + 预算旋钮（F18 运营决定，§4.1）**：`query()` 现超时臂用 `tokio::time::timeout(QUERY_TIMEOUT, cmd.output())`——tokio `Child` 默认 `kill_on_drop=false`（1.52.3 源码实证），**超时丢弃未来后 psql 子进程继续运行**（继续扫表、占连接；过悬崖库上每次超时泄漏一个进程）。修正：`cmd.spawn()` → `timeout(budget, child.wait_with_output())` → 超时臂 `child.kill().await` + `child.wait().await`（杀进程 + 回收防僵尸）。预算旋钮：`QUERY_TIMEOUT` const 降为默认值，实际预算经 `query_timeout()` 读 `AERO_ENG_QUERY_TIMEOUT_SECS`（模块本地单下划线约定，同 `AERO_PSQL_MODE` / `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE`；缺省/不可解析/0 → 30s，fail-closed 方向——错误旋钮不得静默缩小预算）。合计 ~6 行，仍落 ≤13 行 runner delta（§2.3）。

### 2.2 `audit_provision.rs` 接线 delta（≤13 行）

1. `AuditSnapshot` 增 `pub coverage: Option<CoverageCounts>`（表缺席 → `None`，镜像 `g0239` Option 语义）。
2. `verdict()`：dead 分支之后、relay-health 分支之前插入 4 行：

```rust
if let Some(c) = &s.coverage {
    if let Some(reason) = crate::audit_coverage::coverage_verdict(c, s.relay_enabled) {
        return Verdict::FailClosed(reason);
    }
}
```

3. `format_report()`：`Some(g)` 分支之后、priority 行之前插入 3 行：

```rust
if let Some(c) = &s.coverage {
    out.push_str(&crate::audit_coverage::format_lines(c, s.relay_enabled));
}
```

4. `run()` 的 `Some(table)` 臂内、Q5 块之后、P 之前插入 4 行：

```rust
let coverage = match crate::audit_coverage::probe(&runner, table).await {
    Ok(c) => Some(c),
    Err(e) => return outcome_error(e),
};
```

5. 快照构造补 `coverage,` 一行。`lib.rs` 增 `pub mod audit_coverage;`（与 `pub mod audit_provision;` 并列）。

探针顺序钉死：Q6 → Q7 → Q8 → Q9 → Q10（Q5 之后、P 之前）。表缺席 → 不跑任何 coverage 查询、无 coverage 报告行、verdict 不受影响（`outbox-0239: not migrated` 同款静默）。

### 2.3 实现预算

| 项 | 行数 |
|---|---|
| `audit_coverage.rs`（consts + structs + 6 纯函数 + `probe_snapshot_sql` + `parse_probe_output` + probe） | ≤190（目标 ~185；D9 增量：builder ~12 + 路由 ~30 − probe 简化 ~10） |
| `audit_provision.rs` delta（2 处 `pub(crate)` + D10 超时臂 ~4 行 + 旋钮 ~2 行） | ≤13 |
| `tests/audit_provision.rs` 追加 | ~90 |
| `test-integration.sh` 腿 C | ~110 |
| `b5-pin.sh` / `test-b5-pin-guard.sh` 换槽 | 3 处 + 2 串 |

## §3 兼容性约束

- **CLI 面零扩张**：`main.rs` help/cmds 串（`:316`）与命令臂（`:489-506`）不动；`--priority` 经 `run_priority → run()` 自动继承 coverage 面（base error 先返回，drill 不 spawn）。`tests/cli_smoke.rs` 以「无触碰」保证绿。
- **报告行兼容**：既有行（`relay:` / `v1-outbox:` / `outbox-0239:` / `oldest-pending-age:` / `dead:` / `priority:` / `class:` / `verdict:`）逐字节不动；新行前缀 `audit-provision-check: coverage:` 与 `coverage-classes:`。**前缀碰撞分析**：现有测试断言 `audit-provision-check: class: landed|absent`（tests :312/:317 子串）——`coverage-classes: admin=…` 不含 `class: landed` 子串；harness 现有 grep（`verdict: / outbox-0239: / dead= / delivered= / oldest-pending-age:`）零命中新行。
- **verdict 矩阵语义**：dead-first 优先级不动（dead 仍压过 coverage 违约）；coverage 违约仅**新增** FailClosed 分支；`Consistent`/`Healthy` 语义逐字节不动；exit 契约（fail-closed=1 / ok=0 / priority SKIP=2）不动。
- **零 DDL / 零迁移 / 零新依赖**：全部新探针 SELECT-only；`aero-eng/Cargo.toml` 不动；不链接 connector / aero-storage / aero-ai（`AuditClass` 经已有 aero-common 依赖）。
- **字面量纪律**：`admin.content.flag` 零出现（truth-check AUDIT-FLAG）；`message.moderated` 仅 SQL const（无 mirror，合法）；class 标签经 `AuditClass::as_str()`（填充 `ClassCounts` 与报告行均经枚举，无裸字面量）。
- **可见性纪律**：`unreachable_pub = "warn"`——SQL consts / `q8_orphan_sql` / `probe` 为 `pub(crate)`（仅 crate 内消费）；解析器 / verdict / format_lines / structs 为 `pub`（集成测试消费）。零调用函数不做（truth-check 零调用守卫）。
- **37/37 pin 逐字节不动 guard**：`assert_b5_contract_pin` 代码零改动；仅换 `contract-test-22[PROPOSED]` → `audit-governance-coverage`（过 `^[A-Za-z0-9_:+-]+(\[PROPOSED\])?$`），总数保持 37。
- **并行集成**（与 direction #1 同批）：各自新兄弟模块（`audit_class_report.rs` / `audit_coverage.rs`）互不触碰；共享文件仅 `lib.rs`（两个 `pub mod` 行）与 `tests/audit_provision.rs`（各自追加）；`audit_provision.rs` delta 面不重叠（#1 动 `G0239Counts`/Q6 执行留在 run() 内，本设计动 `AuditSnapshot`/coverage 分支）。**`PsqlRunner` 的 `pub(crate)` 化两方向共享，先落地者做，后落地者复用**——合入时 `cargo check --workspace` 验证。

## §4 失败模式（fail-closed 窗口语义全表）

| # | 场景 | 行为 |
|---|---|---|
| F1 | 任意 Q6-Q10 查询失败（ON_ERROR_STOP / psql 缺失 / DB 不可达 / 超时） | 五语句同在一个 `-c`：任一失败 → 整调用非零 → `probe` → Err → run() `outcome_error` → exit 1（与 Q3/Q4/Q5 同契约）；超时预算（默认 30s，`AERO_ENG_QUERY_TIMEOUT_SECS` 可调，五语句共享）**单次尝试、零自动重试、零退避**；hang vs error 在报错串可区分（`timed out after …` vs `psql failed (exit N)`）；超时臂杀子进程 + 回收（D10）——契约全文见 §4.1 |
| F2 | `parse_dedup_line` 缺 cell / 垃圾（`x\|2` / 空串） | Err → fail-closed exit 1 |
| F3 | `parse_class_counts` 坏行 / 未知 class | 忽略（`parse_buckets` fail-open 扫描哲学；未来 lane 经迁移落地，扫描工具容忍）；ON_ERROR_STOP 已前置失败查询 |
| F4 | 0239 表缺席 | 无 coverage 查询/行，verdict 不受影响（不印噪音） |
| F5 | relay 关 + missing>0 + orphans=0 + dedup=f | 印 `fail-open-window: <M> audit-only row(s) — 0241 reconciles on re-enable`；exit 按既有矩阵（无 undelivered → consistent / 0）——**部署库曾关开关不假红**（A2 half 4） |
| F6 | relay 关 + missing>0 + orphans>0 | `in-tx-broken`（孤儿判据不依赖 relay——孤儿来源是 audit 侧删除，与 runtime 开关无关） |
| F7 | 窗内孤儿（created_at ≥ now-N 天） | fail-closed（无设计来源——留存只删 `created_at < now-N`，窗内孤儿不可能被 sweep 合法删除） |
| F8 | 窗外孤儿 | 报告计数（`orphans=<R>`）但不 fail-closed（设计态：审计硬删 + outbox 行作 durable cursor 长寿） |
| F9 | 留存 env 未设 / 不可解析 | 默认 365；负值 → 0（无界窗，保守 fail-closed 方向） |
| F10 | 重复 id 第二发（复合 PK 容许，ON CONFLICT 吞） | Q7 → `t` → fail-closed（任意 runtime 态；reason 含 "duplicate audit id"） |
| F11 | dead 行 + coverage 违约并存 | dead 优先（verdict 顺序：dead → coverage → relay-health） |
| F12 | binding-less 工作区的 message.moderated audit 行 | 不进 Q6 缺失计数（enabled-binding join = 0241 mapped subset；audit-only 是 gate 的 fail-open 设计语义） |
| F13 | `--priority` 面 | base run() 先 fail-closed → drill 不 spawn（`base.is_error()` 早退） |
| F14 | 多 binding 扇出 | 不存在：`snaplink_commercial_bindings.workspace_id` 是 PK，LEFT JOIN 不放大 |
| F15 | 时区/时钟域 | Q8 用 `clock_timestamp()`：RR 快照**不冻结**它（执行时时钟），窗口边界 = 探针执行瞬间——sweep 语义正确（`created_at < now-N` 才可被合法硬删，窗内孤儿不可能合法产生）。时钟域统一：audit DEFAULT `now()`（0146:92，事务时）/ outbox DEFAULT `clock_timestamp()`（0239:46，语句时）/ 腿 C 显式 `clock_timestamp()+1s` / Q8 窗口均同一 DB 服务器时钟（两 DEFAULT 冻结点差 µs 级，365 天窗下无影响；Q4 先例） |
| F16 | 活库并发提交使五计数互相矛盾（未加单快照包裹时的假红/假绿） | **设计消除（D9）**：显式 `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY` 单 `-c` → 五计数同一快照（实证 E11：无显式隔离时并发插入 1.5s 内 `1`→`2`；RR RO 后恒 `2`/`2`）。防回归理解：READ ONLY + MVCC 零锁，不阻塞 relay 路径 |
| F17 | INTERVAL 溢出（极端 `AERO__SERVER__AUDIT_RETENTION_DAYS`） | 活库实证（§9.5）：`INTERVAL '2147483648 days'`（int32+1）→ `interval field value out of range`；`clock_timestamp() - INTERVAL '3650000 days'`（1 万年）→ `timestamp out of range`（timestamptz 下限 ~4713 BC，安全天花板 ≈245 万天）。可解析的极端 env 值（如 `999999999999`）会让 Q8 查询失败 → `probe` Err → 误 fail-closed，且报错信息是 psql 错误而非配置问题。**修复**：`audit_retention_window_days()` 返回前 clamp 上限 365_000（1000 年，仍低于安全天花板 7×）；负值 → 0 不变 |
| F18 | 探针全表扫 vs 30s `QUERY_TIMEOUT` 悬崖 | **运营决定见 §4.1**：目标库由 `DATABASE_URL`/`AERO__DATABASE__URL` 决定、base check **无 throwaway 守卫**——CI/harness 一次性库下**有界（accept-with-limit）**；长活部署库下**真实（accept-with-mitigation）**。悬崖：Q6/Q7/Q8 全窗扫描无分区裁剪（§9.2 EXPLAIN），审计侧合计 ~383µs/千行（102/127/154）+ Q9/Q10 outbox 侧 ~15–20% → 默认 30s ≈ **~58–75M audit 行（审计侧单独 ~78M）**；5 亿行 ≈ **191s ≈ 6.4× 预算**（Q6 51s / Q7 64s / Q8 77s，各自已超）。留存 DROP 把表界到 N 天但**不等于小**（1.4M 行/天 × 365 ≈ 5 亿）。缓解（零迁移）：旋钮 `AERO_ENG_QUERY_TIMEOUT_SECS` + 超时杀子进程（D10）+ 可区分报错串 + 假红包络/rerun 政策；**否决** Q8 审计侧窗谓词（稳态零增益、days=0 语义必需无界）与 action 部分索引（=迁移，禁区） |
| F19 | 留存 sweep（行删+分区 DROP）与探针并发 | **无竞态（已证，§4.1 证明）**：行删 `created_at < cutoff` 严格 `<` + legal-hold 豁免（audit.rs:64-81）；分区 DROP 按日粒度严格 `< today−N` + 逐分区 legal-hold 检查（0154）→ 边界行（= now−N）两条路径都存活，其孪生必在 Q8 窗内 → 绿；RR 单快照把 sweep 提交前/后状态完整呈现（前：行可见→无孤儿；后：被删行恒在探针窗外→不计数）；残余界 = 播种事务开启 > N 天（行 DEFAULT `now()`=事务起点 < S−N 而 INSERT 语句落窗内、探针快照紧跟 sweep 提交 δ 内）——本系统全部 audit 写入者皆短事务，365 天开事务运行期不可达；反方向 READ ONLY + MVCC 零锁，探针不可能干扰 sweep |

### 4.1 F18 运营决定（target-DB 判定 + 悬崖量化 + 缓解 + 探针超时契约 + sweep 交互证明）

**目标库判定**：门读 `DATABASE_URL`/`AERO__DATABASE__URL`（main.rs 命令臂，base check **无任何 throwaway 守卫**；`--priority` 的 REFUSED 守卫不覆盖 base check）。两个包络：(a) **CI/harness**（leg B / 腿 C：`aero_*_$$` 一次性库，`assert_disposable_db_name` 门控，种子数百~402k 行）→ 远低于悬崖 → **F18 有界，accept-with-limit**；(b) **部署运维**（requirement 的 "on a deployed DB" 即此面）→ 长活库、无守卫 → **F18 真实，accept-with-mitigation**（非 accept-with-limit）。

**悬崖量化**（活库 402k 行实测，§9.2）：Q6 41ms / Q7 51ms / Q8 62ms = 102/127/154µs 每千行；审计侧合计 ~383µs/千行（硬件方差取 300–460µs）。5 亿行：Q6≈51s、Q7≈64s、Q8≈77s，合计 ≈ **191s = 6.4× 默认预算**，且 D9 单 `-c` 下预算对整批生效——5 亿时 Q6 即超（原 F18 行「1 亿行 ≈ 10-15s 绿」是逐查询视角，D9 共享预算下 1 亿 × 383µs ≈ 38s 已红）。**探针有效悬崖**：预算覆盖五语句，Q9/Q10 扫 outbox（与 audit 行 1:1，单表 seq scan ~100–130µs/千行）在同行数下再占 ~15–20% → 有效悬崖 ≈ **~58–75M 行（审计侧单独 78M；按 1.4M 行/天 ≈ 42–54 天达悬崖；100k 行/天则 ~1.6–2.1 年）**。留存 DROP 仅把表界到 N 天（5 亿 ≈ 365×1.4M），"有界 ≠ 小"。outbox 侧对**既有** Q3/Q4 的单独悬崖 ~5–10 亿行（§9.2）——晚于探针，**探针的审计侧扫描是绑定约束**。

**最小缓解（零迁移、零 CLI 面）**：
1. **预算旋钮** `AERO_ENG_QUERY_TIMEOUT_SECS`（默认 30；缺省/不可解析/0 → 30，fail-closed 方向）：悬崖下库维持 30s 快速暴露真 hang；过悬崖库运维侧调大（5 亿 → ~240s）让门跑完出真 verdict——门不撒谎，只是预算内看不见。
2. **超时杀子进程 + 回收**（D10）：现状 tokio `Child` drop 不杀（`kill_on_drop=false`，1.52.3 源码实证），每次超时泄漏一个继续扫表的 psql；修正后 kill + reap 双保险。
3. **可区分报错串**：hang → `psql query timed out after {budget}: {sql}`；error → `psql failed (exit N): {stderr}`。两者同走 fail-closed exit 1，无部分 verdict。
4. **假红包络 + rerun 政策**：包络 = 全窗扫描超预算的库。超时红与违约红按报错串可区分；rerun 政策：先确认 DB 负载再重跑一次（探针只读幂等，重跑恒安全）；仍超时 → 库已到/过悬崖 → 调旋钮或缩留存（分区 DROP 随即收紧审计侧）；**未跑完的 run 不得当作 breach 证据**。
5. **否决项**：Q8 审计侧窗谓词（`a.created_at >= now−N`）——稳态下 sweep 已把表界到 N 天，谓词零增益；days=0 时无界窗是语义必需（不 sweep ⇒ 每孤儿皆真）。action 部分索引 = 迁移，禁区。

**探针超时/退避/重试契约**：单次尝试、零自动重试、零退避——五语句共享一个预算在单 `-c` 内；重试装不进同一预算，且对已受压的库是二次负荷；验证门确定性优先（同输入同 verdict）。rerun 是操作员动作（政策 4）。

**留存 sweep 交互（F19，证明）**：行删严格 `<`（audit.rs:64-81，legal-hold 豁免）与分区 DROP 按日粒度严格 `< today−N`（0154，逐分区 legal-hold 检查）使边界行（created_at = now−N）两条路径都存活；其 outbox 孪生（同事务 AFTER INSERT trigger，`clock_timestamp()` ≥ 行值）必在 Q8 `>=` 窗内 → 反连接命中 → 绿。RR 单快照把并发排除在结构外：sweep 提交在快照前 → 完整可见——被删行 `created_at < S−N < P−N`（S=删除语句时、P=探针快照时）⇒ 孪生恒在窗外（除非 δ = 语句时−事务起点 > P−S，而 δ>0 又要求播种事务开启时长超过 sweep 提交→探针快照间隙）；sweep 提交在快照后 → 不可见，行仍在 → 无孤儿。残余界：播种事务**开启**于 > N 天前（行 DEFAULT `now()`=事务起点 < S−N）且其 INSERT 语句落在窗内、探针快照紧跟 sweep 提交 δ 内——本系统全部 audit 写入者皆短事务（psql 单 `-c` 种子 / moderation-finalize / 0241 reconcile / connector claim 批，sqlx 池短事务），N=365 天长开事务运行期不可达；PG `idle_in_transaction_session_timeout`（若配置）亦终结之。反方向同理：探针 READ ONLY + MVCC 零锁，不可能拖住或干扰 sweep。

## §5 迁移步骤（零 DB 迁移；代码/纪律迁移序列）

无 `migrations/NNNN_*.sql`——不触发 §4.2「build → migrate」纪律（`make migrate-smoke` 不受影响）。落地顺序：

1. **模块**：写 `crates/aero-eng/src/audit_coverage.rs`；`lib.rs` 加 `pub mod audit_coverage;`。
2. **接线**：`audit_provision.rs` 按 §2.2 五处 delta（含 `PsqlRunner`/`query` 两处 `pub(crate)`）+ D10 超时臂修正（spawn → wait_with_output → 超时 kill+reap）与 `AERO_ENG_QUERY_TIMEOUT_SECS` 旋钮读取（§4.1）。
3. **单测**：`tests/audit_provision.rs` 追加 §6 断言集；既有 `snapshot()` helper 补 `coverage: None` 字段（测试文件在扩展范围，机械补字段，既有 verdict 测试零语义改动）。
4. **换槽**：`b5-pin.sh` `contract-test-22[PROPOSED]`（:67）→ `audit-governance-coverage`；头注释「15 executed + 22 [PROPOSED]」→「16 executed + 21 [PROPOSED]」（:6/:23 两处）；`test-b5-pin-guard.sh` **:64 与 :113** 期望串同步为 `"B5 contract pin: 37/37 (16 executed, 21 \[PROPOSED\]): PASS"`。
5. **腿 C**：`test-integration.sh` leg B 块内 B2 drop 之后、`b5_check "audit-provision-check"` 之前插入（独立 throwaway 库 + 新 slot；0239 缺席 → `b5_check "audit-governance-coverage" "SKIP (0239 not landed)"`）。
6. **验证**（提交前必过）：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `bash scripts/test-b5-pin-guard.sh`（换槽后自测绿）。活验证用全新一次性库（腿 C 内建 CREATE → migrate → DROP 纪律；`AUDIT_PROVISION_COVERAGE_DB="aero_audit_provision_coverage_$$"` 过 `^aero_[A-Za-z0-9_]{1,58}$`）。

## §6 Testable acceptance mapping（acceptance 原句 → 可测断言）

| AC（原句） | 可测断言 | 位置 |
|---|---|---|
| **AC1** LEFT JOIN 缺失探针 nonzero → error；orphan 公式 `COUNT(event_id) != COUNT(DISTINCT event_id)` → error | 缺失探针 Q6（enabled-binding join 收窄）+ `coverage_verdict` relay-开分支 → `Verdict::FailClosed` → exit 1。orphan 公式落 **audit 侧 Q7**（outbox 侧因 PK 结构性死检）：`(COUNT(a.id) <> COUNT(DISTINCT a.id))::text` → `t` → exit 1（任意 runtime 态）。另增 Q8 孤儿反向 join（留存窗收窄）。断言：单测矩阵（dedup×2 / orphans×2 / missing×2 全分支）+ 腿 C 步 5（重复 id 种子 → exit 1 + `in-tx-broken` + `duplicate audit id`；清理 → exit 0 + `in-tx-ok`）+ 步 6（窗内孤儿种子 → exit 1 + `in-tx-broken` + `orphan`；清理 → exit 0） | R2/R3/R5/R8（req spec） |
| **AC2** `'audit-provision-check: coverage: in-tx-ok'` / per-class counts / 解析器单测 | `format_lines` 三行精确输出单测：`coverage: audit=<A> outbox=<O> missing=<M> orphans=<R>` / `coverage-classes: admin=<a> message=<m> room=<r>` / 第三行 `in-tx-ok` ↔ `in-tx-broken: <join>` ↔ `fail-open-window: <M> audit-only row(s)` 与 `coverage_verdict` 矩阵**同构**（共享 `breach_components`，测试对表钉死）；`parse_dedup_line`（`t\|2`/`f\|0`/空白容忍；缺 cell/`x\|2`/空串 → Err）；`parse_class_counts`（多行/空串→空/坏行忽略/未知 class 忽略）；腿 C 步 4 grep `coverage: in-tx-ok` + `verdict: healthy` | R4/R5/R8 |
| **AC3** 腿 C：seed 一条 `message.moderated` audit 行经 trigger 路径（binding 在 + runtime 开）→ outbox 行同事务存在；binding 缺失 → 事务 abort（audit 行也缺席） | 腿 C 步 3：固定 UUID 种子（`…c0` participant / `…c1` workspace / `…c2` audit id / `…c3` target）+ psql 断言 `COUNT(*) FROM audit_governance_outbox WHERE event_id='…c2'` == 1（同事务 outbox 行直接证据）；步 4：check → exit 0 + `in-tx-ok`；步 7：删 binding → 同款 INSERT **必须失败**（`if run_psql …; then 腿 FAIL; fi` 断言 Gate 2 P0001）+ `COUNT(audit_events)=1` / `COUNT(outbox)=1` 零逃逸断言（'g' < 's' 触发序：RAISE 先于 v1 副作用）；0239 缺席 → SKIP-with-reason | R6 |
| **AC4** 37/37 pin：新 slot 只经 `assert_b5_contract_pin` 加入；count==37 guard 绿 | `B5_CONTRACT_TEST_LIST` 换入 `audit-governance-coverage`（替换 `contract-test-22[PROPOSED]`，总数仍 37 = 16 executed + 21 [PROPOSED]）；`assert_b5_contract_pin` 代码零改动；`test-b5-pin-guard.sh` :64/:113 期望串同步 → 自测绿；腿 C 发 `B5-CHECK audit-governance-coverage: PASS|SKIP` 证据行（`b5_check` 追加 B5_LOG，fresh 模式每个 executed 槽 ≥1 条） | R7 |

**单测清单**（`tests/audit_provision.rs` 追加，~90 行）：
- `parse_dedup_line`：`"t|2"` → `(true, 2)`；`"f|0"` → `(false, 0)`；空白容忍；缺 cell / `"x|2"` / 空串 → `Err`。
- `parse_class_counts`：三行常规 → 3 对；空串 → 空；坏行（无 `|` / 非数字 count）忽略；未知 class（如 `"future"`）保留在 Vec 但填充 `ClassCounts` 时被忽略（缺 → 0）。
- `q8_orphan_sql`：`days=365` → 含 `INTERVAL '365 days'`；`days=0` → 无 `AND` 子句；`{table}` 替换正确。
- `probe_snapshot_sql`：含 `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY` 与 `COMMIT`；五语句首列 `'Qx' AS probe_tag`；`{table}` 替换；`days=0` 无窗子句。
- `parse_probe_output`：tag 路由（`Q7|t|2` → dedup 违约、`Q6|3`/`Q10|5`/`Q8|0` + Q9 缺节 → 全零 classes）；`BEGIN`/`COMMIT`/空行跳过；Q6/Q7/Q8/Q10 缺节或超一行、未知 tag 行 → `Err`（fail-closed）。解析器契约不变：`parse_dedup_line`/`parse_class_counts` 仍收无 tag 行（tag 只在路由层剥离）。
- `coverage_verdict` 4×2 矩阵：dedup=t × relay 开/关 → `Some`（含 "duplicate audit id"）；orphans>0 × relay 开/关 → `Some`（含 "orphan"）；missing>0 × relay 开 → `Some`（含 "missing their 1:1"）、× relay 关 → `None`（**关窗豁免钉死**）；全零 × relay 开/关 → `None`。
- `format_lines`：干净态三行精确匹配；违约态第三行 `in-tx-broken: duplicate audit id(s)…`；关窗态 `fail-open-window: 2 audit-only row(s)…`；`coverage-classes` 顺序恒 `admin message room`。
- `format_report` 集成：`coverage: Some` → 报告含三行；`None` → 无 coverage 行（`report_not_migrated_has_no_age_or_dead_lines` 同款追加）；既有 verdict/report 测试只机械补字段。

## §7 腿 C 规格（`scripts/test-integration.sh`）

```
位置：leg B 块内、B2 的 drop_created_database 之后、b5_check "audit-provision-check" 之前
新 DB：AUDIT_PROVISION_COVERAGE_DB="aero_audit_provision_coverage_$$"（:38 区 + assert_disposable_db_name 条目）
门控：grep -q "audit-provision-check" <<<"$B5_HELP_OUT" ∧ [ -f migrations/0239_audit_governance_outbox.sql ]
     任一缺失 → b5_check "audit-governance-coverage" "SKIP (<reason>)"
1. create_throwaway_database + aero-cli migrate（leg B 同款）
2. （可选 sanity）空库 check → exit 0 + verdict: consistent
3. 种子（run_psql -v ON_ERROR_STOP=1 多语句；列集照抄 audit_governance.rs fixture :120-160）：
   INSERT participants ('…c0','human','leg-c-actor');
   INSERT workspaces ('…c1','Leg C WS','leg-c-ws','…c0');
   INSERT snaplink_commercial_bindings ('…c1','tenant-leg-c','client-leg-c','audit-client-leg-c','source-leg-c',1,TRUE);
   UPDATE snaplink_commercial_runtime SET enabled=TRUE, updated_at=clock_timestamp() WHERE singleton;
   INSERT audit_events (id,workspace_id,action,target,detail)
     VALUES ('…c2','…c1','message.moderated','…c3','{"reason":"leg-c"}'::jsonb);
   psql 断言：SELECT COUNT(*) FROM audit_governance_outbox WHERE event_id='…c2' == 1
   断言语义（快照/时钟域确认，D9）：断言在**种子提交后**以独立 psql 会话运行，观察已提交状态——它验证的是种子事务的原子性（AFTER INSERT trigger 同事务写 outbox 行；多语句种子单 `-c` = psql 隐式单事务，ON_ERROR_STOP=1 任一失败整块回滚），**不**与探针共享快照（探针尚未运行；顺序执行无竞态）。断言本身 `-v ON_ERROR_STOP=1` 且输出比对 ==1，否则腿 FAIL（镜像 leg B 断言风格）。时钟域统一：种子 DEFAULT/显式时间戳与探针 Q8 窗口同一 DB 服务器时钟（F15）。
4. check → exit 0 + grep 'coverage: in-tx-ok' + grep 'verdict: healthy'
5. Q7 端到端：DELETE FROM snaplink_delivery_outbox WHERE destination='audit' AND idempotency_key='…c2'
   （v1 UNIQUE(destination,idempotency_key) 会 abort 第二发——db_test duplicate 同款前置清理，§4.5 D8）
   → INSERT audit_events (…'…c2',…,'message.moderated',gen_random_uuid()::text,'{"reason":"leg-c-dup"}'::jsonb,
     clock_timestamp()+interval '1 second')
   → check **exit 1** + grep 'in-tx-broken' + grep 'duplicate audit id'
   → DELETE FROM audit_events WHERE id='…c2' AND detail->>'reason'='leg-c-dup'（detail 标记唯一命中）
   → check exit 0 + 'in-tx-ok'
6. Q8 端到端：INSERT audit_governance_outbox (event_id,payload) VALUES ('…c4','{}'::jsonb)
   （created_at=now()，窗内；trigger 产物是 16 键信封，payload '{}' 唯一命中）
   → check **exit 1** + grep 'in-tx-broken' + grep 'orphan'
   → DELETE FROM audit_governance_outbox WHERE payload='{}'::jsonb → check exit 0 + 'in-tx-ok'
7. 原子性：DELETE FROM snaplink_commercial_bindings WHERE workspace_id='…c1'
   → 同款 INSERT audit_events（gen_random_uuid() id, reason 'leg-c-abort'）**必须失败**
     （if run_psql …; then 腿 FAIL; fi——Gate 2 P0001，'g' 先于 's'）
   → psql 断言 COUNT(audit_events ws='…c1' ∧ action='message.moderated')==1（abort 零逃逸）
     + COUNT(outbox event_id='…c2')==1（治理行未被污染）
   → 不再跑绿 check（该态 relay 开 + 0 binding + undelivered → 既有矩阵 fail-closed 是期望）
8. drop_created_database + b5_check "audit-governance-coverage" "PASS"
```

## §8 协调 & 硬规则（AGENTS §4）

- 零 CLI 面改动；`cli_smoke` 无触碰绿。
- 非破坏性：全部新探针 SELECT-only；`run_priority` 的 drill 破坏门（D8′）不动。
- 字面量纪律：`admin.content.flag` 零出现；`message.moderated` 仅 SQL const；class 名经 `AuditClass::as_str()`。
- 环境变量：`AERO__SERVER__AUDIT_RETENTION_DAYS`（双下划线；read 失败/缺省 → 365，cross-pin `retention.rs:24`）。
- 尺寸纪律：新代码全落 `audit_coverage.rs`（≤190）；`audit_provision.rs` delta ≤13（远低于 800 WARN 余量）；禁止顺手拆分既有代码。
- aero-eng 零 DB 依赖：探针走 PsqlRunner（psql 子进程）；词表经 aero-common。
- 活验证：全新一次性库（CREATE → migrate → DROP；腿 C 内建）；`make migrate-smoke` 不受影响（零迁移改动）。
- 提交前必过：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets` · `scripts/{truth-check,file-size-check,web-check}.sh` · `bash scripts/test-b5-pin-guard.sh`。

## §9 活库核验附录（真实 schema + EXPLAIN + 边界证明，2026-08-08）

> 一次性库 `aero_q_verify_*`（pg17，docker aero-postgres）：241 迁移全链应用 → 种子 → EXPLAIN/断言 → **DROP**。方法：psql 逐文件（含显式事务控制的文件原样、其余 `-1` 单事务；5 个含顶层 `LOCK TABLE` 的文件须 `-1`——与 sqlx 每文件一事务的语义一致）。种子：403 个日分区（模拟已维护表）、402,000 audit 行（400k 批量散布 400 天 + 2,000 经 trigger 路径活入 w1）、2,000 outbox 行。

### 9.1 F14 实证：workspace_id 是 PK，LEFT JOIN 不扇出

`pg_indexes`：`snaplink_commercial_bindings` PK = `workspace_id`（0235:19 实测）。Q6 设计形态（LEFT JOIN + `b.workspace_id IS NOT NULL`）与 0241 INNER JOIN 形态（:67-68）在活库**逐行同结果**：两者均 `COUNT=53333`。按工作区分桶核对 null 语义：w1（enabled binding）missing=53333 **计入**；w2（binding enabled=FALSE）missing=53334 **不计**（ON 子句 `b.enabled` 失败 → b 列全 NULL → IS NOT NULL 排除）；w3（无 binding）missing=53333 **不计**。`enabled BOOLEAN NOT NULL`（DDL 实测）→ 无三值逻辑陷阱。

### 9.2 分区裁剪 / 索引覆盖（EXPLAIN ANALYZE，402k 行 / 403 分区）

| 查询 | 计划 | 实测耗时 |
|---|---|---|
| Q6 | `Parallel Append`（**全部 403 分区** seq scan，`action` 过滤 40% 行）→ Hash Join(bindings PK) → Nested Loop Anti Join（outbox PK 探测） | 41ms |
| Q7 | `Parallel Append` 全分区 + **Sort**（COUNT(DISTINCT id) 162k 行 quicksort 3MB） | 51ms |
| Q8 | `Hash Right Anti Join`——哈希**整个 audit_events**（402k 行全分区 Append），窗口谓词只过滤 g（2k 行）侧 | 62ms |

- **无分区裁剪**：Q6/Q7 无 `created_at` 谓词、`action` 无索引（audit_events 仅有 pkey + `(workspace_id, id DESC)`）→ 每次探针全窗扫描。规模受留存分区 DROP 约束（0146 `ensure_audit_event_partitions`），但 1.4M 行/天 × 365 天 ≈ 5 亿行时 Q6/Q7/Q8 单查询将超 `QUERY_TIMEOUT=30s` → 假红（F18）。
- **Q8 结构性全扫**：0146 复合 PK `(id, created_at)` 无 id 独立全局索引 → 按 id 探测必须访问每个分区的索引（无分区键无法裁剪）→ 孤儿检测必然触及全窗 audit 行。这是分区设计的固有代价，不是查询缺陷。
- **outbox 侧同样无界**：`audit_governance_outbox` 是 durable 交付账本——生产 connector 只 `UPDATE` 状态（claim :122 / delivered :163,:197,:228；delivered 行保留作游标），`retention.rs` 无该表 sweep → 既有 Q3/Q4（各自独立 30s 预算，悬崖 ~5–10 亿行）与新 Q9/Q10（**在探针共享预算内**随治理事件率增长，同行数下再占 ~15–20%，§4.1）线性扩张（无分区单表 seq scan）。**审计侧（Q6/Q7/Q8）是探针的绑定约束**（§4.1）。
- 计划选择正确性：Q6 反连接走 outbox PK、bindings 走 PK hash——小表侧均有索引命中。

### 9.3 Q8 窗口边界（>= vs >）实证

同语句内铸 cutoff 一次、在**精确边界**种孤儿（`created_at = now - 365 days`）：
`ge_holds=true | gt_holds=false | sweep_would_delete=false`。

- 留存行级清扫（`audit.rs::sweep_before`）是严格 `<`：边界行**不被清扫** → 其孤儿无设计来源 → `>=`（fail-closed）是 `<` 的正确补集；`>` 会在边界造一个假通过空隙。**Q8 的 `>=` 正确。**
- 跨语句微秒竞态（种子用独立语句铸 created_at 时）实测：边界行落后 cutoff 9.45s → 正确判窗外（报告不红）。腿 C 步 6 的种子在探针**前**提交，只会更旧，无假红风险。
- 时序证明（无假红）：被清扫的 audit 行必在删除时刻已 < now−N；探针 cutoff 用自身 `clock_timestamp()`，删除后任何探针的 cutoff 都晚于该行 created_at → 该孤儿恒在窗外。单语句 RR 快照杜绝语句中途删除的交叠（D9）。

### 9.4 时钟域（F15 实证）

DDL 实测**两个**时钟域：`audit_events.created_at DEFAULT now()`（0146，事务时）/ `audit_governance_outbox.created_at DEFAULT clock_timestamp()`（0239，语句时）。Q8 谓词只比较 **outbox 侧**（clock_timestamp 域）对 `clock_timestamp()` → 域内自洽；`audit_events.created_at`（now() 域）不进 Q8 窗口谓词。Q4_SQL 先例确认（`clock_timestamp() - min(available_at)`）。

### 9.5 INTERVAL 溢出实证（F17）

| 输入 | 结果 |
|---|---|
| `INTERVAL '2147483648 days'`（int32+1） | ERROR `interval field value out of range` |
| `INTERVAL '9223372036854775807 days'`（i64::MAX） | ERROR `interval field value out of range` |
| `clock_timestamp() - INTERVAL '2147483647 days'` | ERROR `timestamp out of range` |
| `clock_timestamp() - INTERVAL '100000000 days'`（27 万年） | ERROR `timestamp out of range` |
| `clock_timestamp() - INTERVAL '3650000 days'`（1 万年） | ERROR `timestamp out of range` |

安全天花板 ≈ 245 万天（~6738 年，timestamptz 下限）。→ `audit_retention_window_days()` 必须 clamp（§2.1 已加），否则极端 env 值 = 假 fail-closed。

### 9.6 腿 C 种子修正确认

- 0200 `workspace_birth_owner_commit_guard` 是 **DEFERRED** constraint trigger：workspace + owner membership 必须同事务。活库实证：autocommit（stdin 逐语句）下 workspaces INSERT 必被 `workspace_owner_required` RAISE；`BEGIN/COMMIT` 包裹后绿。**腿 C 的「多语句种子单 `-c` = psql 隐式单事务」（D9 注释）是必要条件**——若实现时改用 stdin/heredoc 逐语句即碎，须保持单 `-c`。
- 种子列集 `audit_client_id` 只在 0237 之后存在（0235 原 DDL 无此列；0237 `ADD COLUMN` + backfill + UNIQUE）——腿 C 全链迁移后列集成立，storage fixture 同款。workspace_members 需 `role='owner'` 行（`is_guest` 默认 FALSE 满足「非 guest owner」）。
- Q7 端到端实证：复合 PK 容许同 id 二次 INSERT（`created_at+1s`）→ 0239 `ON CONFLICT (event_id) DO NOTHING` 吞第二发（outbox 仍 1 行）→ Q7=`t`。v1 前置清理（D8）必需：第二发前 v1 行存在（`DELETE 1` 命中），不删则 v1 `UNIQUE(destination, idempotency_key)` 先 abort。

### 9.7 干净态数字（供腿 C 断言对照）

w1 enabled-binding：missing=53333（批量种子的 audit-only 行，relay 开时 fail-closed 是设计态——0241 首个 claim tick 回填）；Q7=`f|162000`（162k message.moderated = 40%×400k + 2k 活入）；Q8 无孤儿=0；Q9 全 `admin:2000`；Q10=2000。
