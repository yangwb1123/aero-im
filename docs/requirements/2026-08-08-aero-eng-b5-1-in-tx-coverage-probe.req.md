# Requirements Spec — aero-eng B5-1：in-tx coverage/consistency probe（audit_events ↔ outbox 1:1 覆盖探针，落家 = `crates/aero-eng`）

- **Module (analysis root)**: `crates/aero-eng` — `audit_provision.rs`（B5-4 psql-backed fail-closed 供给门）；本 direction 交付物 = 基础 `run()` 内的 1:1 覆盖探针（缺失/孤儿/重复 event_id + per-class 计数）+ harness 腿 C + 单测
- **Direction**: "In-tx coverage/consistency probe for B5-1 — audit_events ↔ outbox 1:1 and same-transaction atomicity check on a deployed DB"（value 8 / risk_reduction 8 / effort 4 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-eng-src-735c8e10.json`（direction #2）
- **Gate anchor**: `docs/campaigns/implementation-gate.md`（G6 "37/37、T-11、moderation 优先级"）
- **Sibling specs（同模块/同批次，命令面协调）**: `2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`（命令本体 = 本 direction 的扩展宿主）、`2026-08-08-aero-eng-b5-1-operation-class-coverage-leg.req.md`（同模块同批次 direction #1：Q6 class/action 报告行 + 腿 B3——两 spec 均走基础 `run()`，若并行落地各自新兄弟模块，互不触碰）、`2026-08-07-aero-cli-b5-acceptance-gate-harness.design.md`（37/37 pin 机制）
- **Status**: Requirements（下述证据全部经源码 grep 核对；**direction 三处事实性更正/细化**——① `audit_events` 是 0146 分区表、PK 是 `(id, created_at)` 复合键，`audit_events.id` 在 DB 层**不唯一**，0239 outbox 侧 `event_id` 是 PK ⇒ acceptance 的 orphan 公式 `COUNT(event_id) != COUNT(DISTINCT event_id)` 若按 outbox 侧字面实现**结构性永不触发**，必须落在 coverage join 的 audit 侧才可探测真实失败形态（§1.1-A）；② 运行时关窗（A2 half 4）与无 binding 工作区（fail-open 语义）下「audit 行无 outbox 行」是**设计态**，缺失探针必须按 enabled-window + enabled-binding 作用域 fail-closed，否则部署库必假红（§1.1-B）；③ 审计留存硬删（`AERO__SERVER__AUDIT_RETENTION_DAYS`）会让 outbox 行合法地失去源 audit 行——孤儿探针必须按留存窗口收窄（§1.1-C）。§1 逐条更正，acceptance 原句在 §5 保留并 re-ground）
- **Verification date**: 2026-08-08。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `migrations/0239_audit_governance_outbox.sql`（`event_id = audit_events.id` 1:1，`aero_enqueue_governance_audit` trigger 带 token-keyed 映射 + binding fail-closed RAISE；gate-1 fail-open 运行时开关） | ✅ 全中。`event_id UUID PRIMARY KEY`（= audit_events.id，1:1，注释钉死 "UNIQUE(event_id)" dedup 契约）；trigger `aero_enqueue_governance_audit`：Gate 1（runtime.enabled 关 → `RETURN NEW`，fail-open）、token-keyed 映射（`IF NEW.action <> 'message.moderated' THEN RETURN NEW`——**只有** moderation token 进治理 outbox，其余 action 静默走 0236 v1 路径）、Gate 2（`binding := aero_snaplink_binding_for_workspace(NEW.workspace_id)`，0235:193-205 该函数在无 enabled binding 时 `RAISE EXCEPTION 'commercial binding is unavailable' ERRCODE 'P0001'`——abort 整个 tx）、`ON CONFLICT (event_id) DO NOTHING` 幂等。`audit_events` 上仅两个 AFTER INSERT trigger（0236 `audit_events_snaplink_delivery` + 0239 `audit_events_governance_enqueue`，'g' < 's' 触发序，全仓 grep 确认） |
| E2 | `migrations/0236_snaplink_governance_reconciliation.sql`（v1 snaplink_delivery_outbox trigger 路径承载非治理 action） | ✅ `aero_enqueue_snaplink_audit` 全 action 入 `snaplink_delivery_outbox`（destination='audit'，`UNIQUE(destination, idempotency_key)`）；同款 Gate 1 + binding 查找（无 binding 时 v1 也 abort——`tenant_id/client_id/source_system` 均 NOT NULL，0235:161-184）。同事务双 trigger 并存 = 「每 audit 行至多两条 outbox 行（v1 全量 + v2 治理子集）」的既有形态 |
| E3 | `crates/aero-eng/src/audit_provision.rs`（Q1_SQL/Q3_SQL probe + PsqlRunner 模式） | ✅ 全中。`PsqlRunner` :437（local/container 双模 + `AERO_PSQL_MODE`/`AERO_POSTGRES_CONTAINER` env + 30s 超时 + ON_ERROR_STOP=1 + `-At` 单值输出）；`run()` :552（Q0→Q1→Q2→Q3/Q4/Q5→P 顺序，`{table}` 只替换 [`G0239_CANDIDATES`] 固定候选字面量）；`run_priority()` :665（先跑 `run()`，天然继承新探针）；`verdict()`（dead-first fail-closed 矩阵）；`parse_buckets`/`parse_v1_line`/`parse_priority_probe`/`parse_dead_rows`/`parse_outbox_count` 解析器先例。**文件 790 行**（`wc -l` 实测；800 WARN / 1200 HARD）——新生产代码须落兄弟模块（§4 R1） |
| E4 | `crates/aero-storage/src/audit_governance.rs`（ddl_contract_defaults_and_checks db_tests——既有 cross-pin，非 CLI 面） | ✅ 全中，且**不止一个** db_test：`moderation_finalize_outbox_parity`（**in-tx oracle**：commit half 同事务 audit+outbox、rollback half 无 binding → 整事务回滚（soft-delete + audit + outbox 一起）、replay half 幂等）、`moderation_finalize_runtime_disabled_commits_1_plus_0`（**A2 half 4：enforcement 关 → audit 行提交、|G|=0——"delivery gate, never an audit-loss gate"**）、`duplicate_event_id_is_deduped_by_on_conflict`（**「`audit_events` 复合 PK `(id, created_at)` 在 DB 层容许 same-id/different-created_at 重复」+ outbox `ON CONFLICT DO NOTHING` 吞第二发**）、`non_moderation_action_passes_through_unmapped`、`governance_reconcile_backfills_disabled_window`（0241 回填：enabled-binding join、NOT EXISTS 扫描、**parity 收敛于 "workspace's mapped subset"**、dead 不复活）。全部 `#[ignore]` PG 门控——正是 direction 所说「只 pin 在 db_tests，工程门看不到」的现状 |
| E5 | `scripts/test-integration.sh:246-281`（audit-provision-check leg B1/B2 模式——fresh DB + psql 断言；leg C 扩展点） | ✅ 位置核对：leg B 块 :250-305（注释 :250、B1 :264-281、B2 :282-301、`drop_created_database` + `b5_check "audit-provision-check" "PASS"` :302-303）；`AUDIT_PROVISION_DB="aero_audit_provision_$$"` :38；`run_psql` :93（`PSQL_MODE` local/container 双模）；0239 文件门先例 :306-324（`audit_governance::` 腿）与 :389-446（T-11 块内同 slot 多腿）。**同 slot 多腿先例成立**（`audit-provision-check` 在 :303/:385/:440 三次 b5_check）——本 direction 走**新 slot**（AC4，§4 R7） |
| E6 | （补充核对）`migrations/0146_audit_events_partition.sql` | ✅ **关键事实**：`audit_events` 0146 起为 RANGE 分区表（按 created_at 日分区 + `audit_events_default` 兜底分区），PK 改为 `PRIMARY KEY (id, created_at)`——`id` 单独**不唯一**。这是 §1.1-A 的 DDL 依据 |
| E7 | （补充核对）`migrations/0241_governance_reconcile.sql` + `crates/aero-server/src/bin/boot/retention.rs` + `crates/aero-storage/src/audit.rs` | ✅ 0241 `aero_reconcile_governance_audit`（connector `pg.rs:91` 每 claim batch 前调用）：token-keyed（`message.moderated` 专属）、enabled-binding join、INSERT 与 0239 trigger 逐字节相同、parity 收敛于 mapped subset。`retention.rs:24` `AERO__SERVER__AUDIT_RETENTION_DAYS`（默认 **365**，`0` 禁用）；`audit.rs:55-71`（storage）审计行超窗**硬删**（legal-hold 工作区豁免）——outbox 行（delivered 留存作 durable cursor）合法地比源 audit 行长寿 ⇒ §1.1-C |
| E8 | （补充核对）37/37 pin 机制 + leaf 词表 + truth-check | ✅ `scripts/b5-pin.sh`：`B5_CONTRACT_TEST_LIST` = 15 executed + 22 `[PROPOSED]`；`assert_b5_contract_pin` :84-131 强校验**恰好 37 槽**（:86-89 `count -ne 37` → FAIL）+ 每个 executed 槽 ≥1 条 `B5-CHECK <name>: PASS|SKIP` 证据行。**`scripts/test-b5-pin-guard.sh` :64/:113 硬编码期望串 `"B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS"`——换槽必须同步改这两处**（§4 R7）。`aero-common/src/model/audit.rs`：`OutboxStatus` :20、`MODERATION_OUTBOUND_ACTION="admin.content.flag"` :150、`LOCAL_ACTION_MODERATED="message.moderated"` :156、`GOVERNANCE_CLASS_*` :164-168（`AuditClass` 枚举 `as_str()` 派生）。truth-check AUDIT-FLAG mirror（`truth-check-lib.sh:278`）只锁 `admin.content.flag`（仅 audit.rs:150 + allowlist）——**`message.moderated` 无 mirror**，SQL const 内嵌该字面量合法（Q1_SQL 内嵌 `'audit'` 同款先例） |

### 1.1 三处事实性更正（direction acceptance re-ground 的 DDL/语义依据）

- **A（orphan 公式的落表）**：acceptance 原句「orphan check `COUNT(event_id) != COUNT(DISTINCT event_id)` → error」——0239 outbox `event_id` 是 **PRIMARY KEY**，按 outbox 侧字面实现**结构性永不触发**（无法种子、无法失败，测试即空转）。唯一能在活库上真正触发的解读：该公式落在 **coverage join 的 audit 侧**（`audit_events.id` 因 0146 复合 PK 而可在 DB 层重复——E6/E4 双重钉死；重复 id 的第二次 fire 被 outbox `ON CONFLICT DO NOTHING` 静默吞掉 = 1:1 契约的歧义/孤儿化，`duplicate_event_id_is_deduped_by_on_conflict` db_test 已 pin 存储层行为）。本 spec 将公式定为 Q7（§4 R3），并在腿 C 以真实种子端到端可测（§4 R6 步 5）。
- **B（enabled-window 豁免）**：acceptance 原句「`LEFT JOIN … WHERE action='message.moderated' AND outbox.event_id IS NULL` — nonzero → error」——若不加作用域，**任何曾关过 runtime 开关的部署库必假红**：A2 half 4（E4）钉死关窗下 audit 提交、|G|=0 是设计态，0241 在 re-enable 后首个 claim tick 回填。同理，无 enabled binding 的工作区 audit 行永久 audit-only（0241："it stays locally audited only, which is the fail-open meaning of the gate"）。⇒ 缺失探针 Q6 必须三条件收窄：`a.action='message.moderated' ∧ g.event_id IS NULL ∧ b.workspace_id IS NOT NULL`（enabled-binding join，与 0241 parity 的 "mapped subset" 语义逐字一致），且 **fail-closed 判定仅在 `relay_enabled` 时生效**；关窗下缺行印 `fail-open-window` 信息行（exit 不受影响）。acceptance 的「nonzero → exit 1」在其自设场景（binding 在、runtime 开——腿 C 的种子即此窗）逐字保留。
- **C（孤儿探针的留存窗口）**：outbox 行合法地比源 audit 行长寿（E7：审计硬删 + delivered 行作 durable cursor）——部署库上「outbox 行无 audit 行」的**设计来源**长期存在。⇒ Q8 孤儿探针按 `AERO__SERVER__AUDIT_RETENTION_DAYS`（默认 365，0=禁用→窗口无界）收窄：`g.created_at >= now() - <N> days` 内的孤儿才是真回归（设计路径不可能删掉这么新的 audit 行）；窗外的孤儿进报告计数但不 fail-closed。

## 2. Verified current state

```
已存在（E1-E8，全部 verified）：
  0239 outbox（event_id PK 1:1 + token-keyed trigger + Gate1 fail-open / Gate2 fail-closed RAISE）
  0241 回填（enabled-binding join，parity 收敛 mapped subset）+ 0236 v1 并存路径
  audit_provision.rs Q0-Q5/P 探针 + dead-first verdict + `audit-provision-check:` 报告行 + PsqlRunner
  storage db_tests（parity/disabled-window/duplicate/reconcile——in-tx oracle 只活在 PG 门控测试里）
  harness 腿 B1/B2（throwaway DB + verdict grep）+ b5-pin.sh 37/37 槽 + test-b5-pin-guard.sh 自测

缺口（本 direction 关闭，全部 verified）：
  a) 工程门只能数 status 桶（Q3）——「audit 行缺 1:1 outbox 行 / 孤儿 outbox 行 / 复合 PK 重复 id /
     trigger/binding-gate 回归」在 Q3 下全绿（E4 的 db_tests 是唯一 pin，非 CLI 面）
  b) 部署库上的 1:1 契约没有任何非破坏性观测面（leg B1/B2 只测 v1 供给门与 fail-closed 矩阵）
  c) 37/37 清单无该契约的命名槽位——回归只能靠 storage 测试发现，工程门静默
```

## 3. Scope

**In scope**：
- 新兄弟模块 `crates/aero-eng/src/audit_coverage.rs`：Q6（缺失）/Q7（重复 id）/Q8（孤儿）/Q9（per-class）探针 + 解析器 + `coverage_verdict` + `format_lines`（R1-R5）
- `AuditSnapshot` 增 `coverage: Option<CoverageCounts>` 字段；`verdict()`/`format_report()`/`run()` 各一处薄接线（R2/R4/R5）
- `crates/aero-eng/tests/audit_provision.rs` 增解析器/verdict/报告行单测（R8）
- harness 腿 C（throwaway DB：trigger 路径种子 → in-tx-ok；重复 id + 孤儿 → fail-closed；binding 缺失 → 事务 abort 且零逃逸行；R6）
- 37/37 pin：`B5_CONTRACT_TEST_LIST` 换入新 executed 槽 `audit-governance-coverage`（替换 1 个 `[PROPOSED]` 占位，总数保持 37）+ `test-b5-pin-guard.sh` 两处期望串同步（R7）

**Out of scope**：
- 0239/0240/0241/0235/0236/0146 DDL、trigger、connector claim SQL、storage 钻——**零改动**
- `verdict()` 既有矩阵语义（dead-first、relay 关 + undelivered fail-closed、consistent/healthy）——**零改动**，只增 coverage 前置分支（R5）
- CLI 面：main.rs help/cmds 串/子命令——**零改动**（无新 flag；`--priority` 经 `run_priority → run()` 自动继承）
- 破坏性操作：全部新探针 SELECT-only，绝无 TRUNCATE/UPDATE/DELETE（drill 的破坏面已有 D8′ 门，不重复）
- 迁移/新表/新索引——零
- 修复探针发现的缺口本身（那是 0239/0241 切片与 ops 的事——探针只负责红/绿 + 可诊断计数）

## 4. Requirements

### R1 — 新兄弟模块 `crates/aero-eng/src/audit_coverage.rs`（尺寸纪律 + 装配）

- `audit_provision.rs` 现 **790 行**（800 WARN / 1200 HARD，`scripts/file-size-check.sh` 实测退出码只数 HARD、WARN 是软信号）——全部新生产代码（5 个 SQL const + 2 个新解析器 + `probe` + `coverage_verdict` + `format_lines`）落新模块；`lib.rs` 增 `pub mod audit_coverage;`（与 `pub mod audit_provision;` 并列）。
- `audit_provision.rs` 内的接线 delta 控制在 ≤20 行：`use` 1 + `AuditSnapshot` 字段 1 + `verdict()` 分支 4 + `format_report()` 分支 3 + `run()` 探针调用 6 + 快照构造 1。若落地后跨过 800 WARN，**记录于实现说明**（HARD 1200 未触即 gate 绿；禁止顺手拆分既有代码——拆分是另一重构，AGENTS §4.2 禁中途叠加）。
- 目标 ≤160 行；`unreachable_pub = "warn"`（root Cargo.toml）——被 `audit_provision.rs`/集成测试消费的项 `pub`，纯内部项私有。新函数全部被调用（truth-check 零调用守卫，不加 allowlist）。

### R2 — Q6 缺失探针（acceptance 主查询，mapped-subset 作用域）

```rust
/// Q6 — governance-lane 1:1 coverage (B5-1): message.moderated audit rows in
/// ENABLED-binding workspaces missing their 1:1 outbox row. The enabled-binding
/// join is the 0241 reconciler's "mapped subset" — binding-less workspaces stay
/// locally audited only (fail-open meaning of the gate, 0241). The runtime-
/// disabled window (A2 half 4: |G|=0 while disabled) is exempted in
/// [`coverage_verdict`], not here — the query reports raw truth.
const Q6_SQL: &str = "SELECT COUNT(*) \
FROM audit_events a \
LEFT JOIN {table} g ON g.event_id = a.id \
LEFT JOIN snaplink_commercial_bindings b \
       ON b.workspace_id = a.workspace_id AND b.enabled \
WHERE a.action = 'message.moderated' \
  AND g.event_id IS NULL \
  AND b.workspace_id IS NOT NULL";
```

- `{table}` 只替换 [`G0239_CANDIDATES`] 解析出的固定候选字面量（Q3/Q4/Q5 同款 `replace` 路径）；`'message.moderated'` 字面量内嵌 SQL const（Q1_SQL 内嵌 `'audit'` 同款先例；truth-check 无该 token 的 mirror，E8；注释 cross-pin `audit.rs:156 LOCAL_ACTION_MODERATED`）。
- 仅在 0239 表存在分支（`Some(table)`）内运行，与 Q3/Q4/Q5 同生命周期；表缺席 → 无 coverage 行、verdict 不受影响（与 `outbox-0239: not migrated` 同款静默，不印噪音行）。
- 查询失败 → 现有 fail-closed 路径（`outcome_error`，exit 1）。

### R3 — Q7 重复 id 探针（acceptance orphan 公式的落表）+ Q8 孤儿探针（留存窗口收窄）

```rust
/// Q7 — 1:1 dedup/ambiguity (the acceptance's COUNT != COUNT(DISTINCT)
/// formula, on the coverage join's AUDIT side): audit_events.id is NOT unique
/// (0146 partitioned composite PK (id, created_at) admits same-id duplicates);
/// the outbox's ON CONFLICT (event_id) DO NOTHING silently swallows the second
/// fire — a duplicated id means two audit rows claiming one outbox slot.
/// (On the outbox side the formula is structurally dead: event_id is the PK.)
const Q7_SQL: &str = "SELECT (COUNT(a.id) <> COUNT(DISTINCT a.id))::text, COUNT(a.id) \
FROM audit_events a \
WHERE a.action = 'message.moderated'";

/// Q8 — orphan outbox rows: outbox rows with no source audit row, scoped to
/// the audit-retention window (AERO__SERVER__AUDIT_RETENTION_DAYS, default 365,
/// boot/retention.rs:24; 0 = retention disabled = unbounded window). Audit rows
/// are hard-deleted past the window (storage/src/audit.rs sweep_before) while
/// outbox rows persist as the durable cursor — an orphan INSIDE the window has
/// no designed source (regression); an orphan OUTSIDE it is the designed
/// retention aftermath (reported, not fail-closed).
const Q8_SQL: &str = "SELECT COUNT(*) \
FROM {table} g \
LEFT JOIN audit_events a ON a.id = g.event_id \
WHERE a.id IS NULL \
  AND g.created_at >= clock_timestamp() - INTERVAL '{days} days'";
```

- Q7 输出 `t|N`（bool 判定 + 总 audit 计数，供报告 `audit=`）；聚合查询恒返回一行（空表 → `f|0`），无空输出分支。
- Q8 的 `{days}` 是 `i64` 解析值格式化成的**数字字面量**（无参数通道、无用户输入进 SQL；`0` → 省略 `AND` 子句，即无界窗口）。窗口来源：`pub fn audit_retention_window_days() -> i64` 读 `AERO__SERVER__AUDIT_RETENTION_DAYS`（`retention.rs:24` 同款 `.ok().and_then(parse).unwrap_or(365)`，默认常量 365 注释 cross-pin）。
- Q8 查询失败 → fail-closed（与 Q6 同契约）。

### R4 — Q9 per-class 计数 + 解析器 + 快照承载

```rust
/// Q9 — outbox per-class lane counts (acceptance "per-class counts"): the
/// 0239 class CHECK vocabulary admin/message/room. Class names print from DB
/// observation; the fixed report order derives from AuditClass::as_str()
/// (aero-common leaf, audit.rs:164-168) — no bare literals in Rust.
const Q9_SQL: &str = "SELECT class, COUNT(*) FROM {table} GROUP BY class ORDER BY class";

/// Q10 — outbox total (1:1 side of the coverage line); {table} candidate.
const Q10_SQL: &str = "SELECT COUNT(*)::bigint FROM {table}";
```

- 新解析器（`audit_coverage.rs` 内，全部纯函数、可单测）：
  - `pub fn parse_dedup_line(line: &str) -> Result<(bool, i64), String>`——Q7 两 cell（`t|2`），复用 `parse_psql_bool_line` 语义；缺 cell/垃圾 → `Err` → fail-closed。
  - `pub fn parse_class_counts(out: &str) -> Vec<(String, i64)>`——Q9 行 `class|count`；空行跳过、坏行忽略（`parse_buckets` 同款 fail-open 扫描；ON_ERROR_STOP 下查询错误已前置失败）；未知 class 忽略（未来 lane 经迁移落地，扫描工具容忍——与 `parse_buckets` 容忍未来 status 同哲学）。
  - 缺失/孤儿计数复用 `parse_outbox_count`（`audit_provision.rs` 现有，`COUNT(*)::bigint` 契约）。
- `pub struct ClassCounts { pub admin: i64, pub message: i64, pub room: i64 }`（经 `AuditClass::as_str()` 派生顺序填充，缺 → 0）+ `pub struct CoverageCounts { pub audit_total: i64, pub outbox_total: i64, pub missing: i64, pub orphans: i64, pub dedup_violation: bool, pub classes: ClassCounts }`。
- `AuditSnapshot` 增 `pub coverage: Option<CoverageCounts>`（表缺席 → `None`，镜像 `g0239` 的 Option 语义）。`tests/audit_provision.rs` 现有构造点机械补 `coverage: None`/`Some`（测试文件在扩展范围）。

### R5 — `coverage_verdict` + 报告行契约 + `verdict()` 接线（fail-closed 窗口语义）

`audit_coverage.rs` 内：

```rust
/// Coverage verdict: None = no breach (or designed fail-open window).
/// dedup_violation and retention-window orphans fail in EVERY runtime state
/// (neither has a designed source — the runtime switch only gates enqueue,
/// never audit-row deletion). missing fails only when the relay is enabled —
/// while disabled, audit-without-outbox is the pinned A2-half-4 design state
/// (|G|=0, 0241 reconciles on re-enable).
pub fn coverage_verdict(c: &CoverageCounts, relay_enabled: bool) -> Option<String>
```

判定矩阵（顺序钉死，全部单测；reason = 命中组件 join，至少一个存在）：
1. `c.dedup_violation` → `Some("duplicate audit id(s) break the 1:1 audit↔outbox mapping (0146 composite PK; ON CONFLICT swallowed the second fire)")`——任何 runtime 态。
2. `c.orphans > 0` → `Some("<R> orphan outbox row(s) inside the retention window")`——任何 runtime 态（孤儿来源是 audit 侧删除，与 runtime 开关无关；窗内孤儿无设计来源）。
3. `relay_enabled && c.missing > 0` → `Some("<M> audit row(s) missing their 1:1 governance outbox row")`。
4. 其余 → `None`（不干预既有矩阵）。

`verdict()`（audit_provision.rs）在 dead 分支之后、relay-health 分支之前插入：

```rust
if let Some(c) = &s.coverage {
    if let Some(reason) = crate::audit_coverage::coverage_verdict(c, s.relay_enabled) {
        return Verdict::FailClosed(reason);
    }
}
```

报告行（`format_report` 在 `Some(g)` 分支之后、priority/class 行之前调用 `format_lines(&coverage)`；前缀 `audit-provision-check:` 保持 harness grep 面）：

```
audit-provision-check: coverage: audit=<A> outbox=<O> missing=<M> orphans=<R>
audit-provision-check: coverage-classes: admin=<a> message=<m> room=<r>
audit-provision-check: coverage: in-tx-ok
```

第三行状态与 `coverage_verdict` 矩阵严格同构（单测对表钉死）：
- `in-tx-ok`：M=0 ∧ R=0 ∧ dedup=f（acceptance AC2 原句 grep 目标）。
- `in-tx-broken: <命中组件 join>`：dedup ∨ R>0 ∨ (relay 开 ∧ M>0)——与 `Verdict::FailClosed` 同触发 → `Outcome::error`（exit 1）。
- `fail-open-window: <M> audit-only row(s) — 0241 reconciles on re-enable`：relay 关 ∧ M>0 ∧ R=0 ∧ dedup=f——**exit 不受影响**（既有矩阵继续判定）。
- 表缺席 → 无 coverage 行（R2）。
- 全部计数是**观察值**（DB 输出），Rust 不拼 `admin`/`message`/`room` 字面量（`AuditClass::as_str()`，E8）；`admin.content.flag` 不出现在任何新代码（truth-check AUDIT-FLAG，E8）。

### R6 — harness 腿 C（`scripts/test-integration.sh`，新 throwaway 库 + 新 slot）

位置：leg B 块内 B2 之后、`b5_check "audit-provision-check"` 之前；但 **独立 throwaway 库**——B2 已把 `AUDIT_PROVISION_DB` 打成 fail-closed 态，腿 C 不能复用它（且腿 C 需要 0239 表）：`AUDIT_PROVISION_COVERAGE_DB="aero_audit_provision_coverage_$$"`（:38 区新增 + `assert_disposable_db_name` 条目，名字过 `^aero_[A-Za-z0-9_]{1,58}$`）。门控：`grep -q "audit-provision-check" <<<"$B5_HELP_OUT"`（命令在）∧ `[ -f "migrations/0239_audit_governance_outbox.sql" ]`（0239 在）；任一缺失 → `b5_check "audit-governance-coverage" "SKIP (<reason>)"`（镜像 :324-325/:445-446 的 SKIP-with-reason 纪律，pin guard 的 executed-slot 证据行要求）。

1. **建库 + migrate**（leg B 同款 `create_throwaway_database` / `cargo run --bin aero-cli -- migrate` 2>&1 | tail -1）。
2. **基线**（可选 sanity）：`audit-provision-check` → exit 0 + `verdict: consistent`（空库、relay 关）。
3. **种子（trigger 路径，binding 在 + runtime 开）**——`run_psql -v ON_ERROR_STOP=1` 多语句块，全形态照抄 storage db_test fixture（`audit_governance.rs` `enable_enforcement_with_binding`/`fixture` 的 proven 列集）。**固定 UUID 种子**（throwaway 专用库无并行碰撞问题，免 bash 捕获）：participant `…c0` / workspace `…c1` / audit `…c2` / target `…c3`（`00000000-0000-0000-0000-0000000000cX` 形）：
   - `INSERT INTO participants (id, kind, display_name) VALUES ('00000000-0000-0000-0000-0000000000c0', 'human', 'leg-c-actor');`
   - `INSERT INTO workspaces (id, name, slug, created_by) VALUES ('00000000-0000-0000-0000-0000000000c1', 'Leg C WS', 'leg-c-ws', '00000000-0000-0000-0000-0000000000c0');`
   - `INSERT INTO snaplink_commercial_bindings (workspace_id, tenant_id, client_id, audit_client_id, source_system, revision, enabled) VALUES ('00000000-0000-0000-0000-0000000000c1', 'tenant-leg-c', 'client-leg-c', 'audit-client-leg-c', 'source-leg-c', 1, TRUE);`（tenant/client/source 全局 UNIQUE，0235——固定值 + throwaway 库，无碰撞）
   - `UPDATE snaplink_commercial_runtime SET enabled = TRUE, updated_at = clock_timestamp() WHERE singleton;`
   - `INSERT INTO audit_events (id, workspace_id, action, target, detail) VALUES ('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000c1', 'message.moderated', '00000000-0000-0000-0000-0000000000c3', '{"reason":"leg-c"}'::jsonb);`——**AFTER INSERT trigger 同事务**产出治理行 + v1 行（'g' < 's' 触发序；`audit_events_default` 分区兜底，0146）。
   - psql 断言 1:1：`SELECT COUNT(*) FROM audit_governance_outbox WHERE event_id = '00000000-0000-0000-0000-0000000000c2'` == 1（**同事务 outbox 行存在**的直接证据）。
4. **覆盖绿**：`audit-provision-check` → exit 0 + grep `audit-provision-check: coverage: in-tx-ok` + grep `verdict: healthy`（acceptance「binding present, runtime enabled → outbox row exists in same tx」）。
5. **Q7 重复 id 端到端**（§1.1-A 的公式可测性证明）：
   - 先 `DELETE FROM snaplink_delivery_outbox WHERE destination = 'audit' AND idempotency_key = '00000000-0000-0000-0000-0000000000c2';`（v1 的 `UNIQUE(destination, idempotency_key)` 会 abort 第二发——db_test `duplicate_event_id_is_deduped_by_on_conflict` 同款前置清理）。
   - `INSERT INTO audit_events (id, workspace_id, action, target, detail, created_at) VALUES ('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000c1', 'message.moderated', gen_random_uuid()::text, '{"reason":"leg-c-dup"}'::jsonb, clock_timestamp() + interval '1 second');`（复合 PK `(id, created_at)` 容许同 id；outbox `ON CONFLICT DO NOTHING` 吞第二发）。
   - 跑 check → **期望 exit 1** + grep `coverage: in-tx-broken`（+ `duplicate audit id`）；`DELETE FROM audit_events WHERE id = '00000000-0000-0000-0000-0000000000c2' AND detail->>'reason' = 'leg-c-dup';` 清重复行（detail 标记唯一命中，绝不动种子行）→ 跑 check → exit 0 + `coverage: in-tx-ok`。
6. **Q8 孤儿端到端**：`INSERT INTO audit_governance_outbox (event_id, payload) VALUES ('00000000-0000-0000-0000-0000000000c4', '{}'::jsonb);`（created_at=now()，留存窗内）→ 跑 check → **期望 exit 1** + grep `in-tx-broken` + grep `orphan`；`DELETE FROM audit_governance_outbox WHERE payload = '{}'::jsonb;` 清孤儿（payload `'{}'` 唯一命中——trigger 产出的治理行是 16 键信封）→ 跑 check → exit 0 + `in-tx-ok`。
7. **原子性（binding 缺失 → 事务 abort，零逃逸）**：
   - `DELETE FROM snaplink_commercial_bindings WHERE workspace_id = '00000000-0000-0000-0000-0000000000c1';`
   - 尝试同款 `INSERT INTO audit_events (id, workspace_id, action, target, detail) VALUES (gen_random_uuid(), '00000000-0000-0000-0000-0000000000c1', 'message.moderated', gen_random_uuid()::text, '{"reason":"leg-c-abort"}'::jsonb);` → **必须失败**（Gate 2 `aero_snaplink_binding_for_workspace` RAISE P0001 'commercial binding is unavailable'，0235:193-205，'g' trigger 先于 's'；`run_psql -v ON_ERROR_STOP=1` 非零即期望——`if run_psql …; then 腿 FAIL; fi` 断言失败路径）。
   - psql 断言原子性：`SELECT COUNT(*) FROM audit_events WHERE workspace_id = '00000000-0000-0000-0000-0000000000c1' AND action = 'message.moderated'` == 1（**abort 的插入没留下 audit 行**）；`SELECT COUNT(*) FROM audit_governance_outbox WHERE event_id = '00000000-0000-0000-0000-0000000000c2'` == 1（治理行未被污染）。
   - **不再跑绿 check**（该态 relay 开 + 0 binding + undelivered → 既有矩阵 fail-closed 是期望，非本腿断言面）。
8. `drop_created_database "$AUDIT_PROVISION_COVERAGE_DB"` + `b5_check "audit-governance-coverage" "PASS"`。

### R7 — 37/37 pin（`scripts/b5-pin.sh` + `scripts/test-b5-pin-guard.sh` 同步）

- `B5_CONTRACT_TEST_LIST`：`contract-test-22[PROPOSED]` → **`audit-governance-coverage`**（executed 槽；名字过 pin 正则 `^[A-Za-z0-9_:+-]+(\[PROPOSED\])?$`）。总数保持 **37**（15 executed + 22 [PROPOSED] → 16 executed + 21 [PROPOSED]）——`assert_b5_contract_pin` 的 `count -ne 37` 守卫**逐字节不动**（acceptance「count==37 guard green」）。
- `scripts/b5-pin.sh` 头注释「15 executed + 22 [PROPOSED]」同步改「16 executed + 21 [PROPOSED]」（两处：:23 与 :31 附近注释）。
- `scripts/test-b5-pin-guard.sh` **:64 与 :113 硬编码期望串** `"B5 contract pin: 37/37 (15 executed, 22 \[PROPOSED\]): PASS"` → `"37/37 (16 executed, 21 \[PROPOSED\]): PASS"`——不改则自测红。
- 腿 C 的 `b5_check "audit-governance-coverage" "PASS|SKIP (<reason>)"` 即 pin guard 需要的证据行（fresh 模式每个 executed 槽必须 ≥1 条）。

### R8 — 单测（`crates/aero-eng/tests/audit_provision.rs` 扩展，~90 行）

- `parse_dedup_line`：`"t|2"`/`"f|0"`/空白容忍；缺 cell、`x|2`、空串 → `Err`。
- `parse_class_counts`：常规多行、空串 → 空、坏行忽略、未知 class 忽略；`ClassCounts` 经 `AuditClass` 顺序填充 + 缺失 → 0。
- `coverage_verdict` 矩阵（§4 R5 全分支，4×2 对表）：
  - `dedup=true` + relay 开/关 → 均 `Some`（FailClosed，reason 含 "duplicate audit id"）；
  - `orphans>0` + relay 开/关 → 均 `Some`（reason 含 "orphan"——孤儿判据不依赖 relay，无设计关窗来源）；
  - `missing>0` + relay 开 → `Some`（reason 含 "missing their 1:1"）；`missing>0` + relay 关 → `None`（**关窗豁免钉死**——A2 half 4）；
  - 全零 + relay 开/关 → 均 `None`（既有矩阵继续判定）。
- `format_lines`：三行存在（`coverage: audit=… outbox=… missing=… orphans=…` / `coverage-classes: admin=… message=… room=…` / `coverage: in-tx-ok`）；违约态第三行 `in-tx-broken: …`；关窗态 `fail-open-window: …`。
- `format_report` 集成：`AuditSnapshot.coverage: Some` → 报告含 coverage 行；`None` → 无 coverage 行（`report_not_migrated_has_no_age_or_dead_lines` 同款追加断言）；现有 verdict 测试零语义改动（只机械补字段）。
- `run()` 无 URL fail-closed 测试保持绿（覆盖层不改变 exit 契约）。

### R9 — 纪律（AGENTS §4）

- **零 CLI 面改动**：main.rs / help / cmds 串不动（E5 已核）；`cli_smoke` 以「无触碰」保证绿。
- **非破坏性**：新探针 SELECT-only；不动 `run_priority` 的 drill 破坏门（D8′）。
- **字面量纪律**：`message.moderated` 只进 SQL const（无 truth-check mirror，E8）；`admin.content.flag` 零出现；class 名经 `AuditClass`。
- **环境变量**：`AERO__SERVER__AUDIT_RETENTION_DAYS`（双下划线，§4.3 前缀规范；read 失败/缺省 → 365，cross-pin `retention.rs:24`）。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规；`file-size-check` 退出码只数 HARD——`audit_coverage.rs` 与 tests 均在限内，`audit_provision.rs` ≤20 行 delta 目标 ≤800，超则 WARN 记录于实现说明）· `bash scripts/test-b5-pin-guard.sh`（换槽后自测绿）。

## 5. Testable acceptance mapping（direction acceptance 原句保留，re-ground 到当前仓态）

| AC（原句） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **AC1** 新 coverage 查询：`LEFT JOIN audit_governance_outbox FROM audit_events WHERE action='message.moderated' AND outbox.event_id IS NULL` — nonzero → `Outcome::error` exit 1（fail-closed）；orphan check `COUNT(event_id) != COUNT(DISTINCT event_id)` → error | **Q6 按 §1.1-B 收窄**（enabled-binding join + enabled-window 判定；acceptance 原查询核心子句逐字保留）：Q6 缺失探针 + `coverage_verdict` relay-开分支 → `Verdict::FailClosed` → exit 1（单测矩阵 + 腿 C 步 6 孤儿/步 5 重复 id 端到端）。**orphan 公式落 audit 侧 Q7**（§1.1-A：outbox 侧因 PK 结构性死检）：`(COUNT(a.id) <> COUNT(DISTINCT a.id))::text` → `t` → exit 1（任意 runtime 态；单测矩阵 + 腿 C 步 5 真实种子）。另增 Q8 孤儿反向 join（留存窗收窄，§1.1-C） | R2/R3/R5/R6/R8 |
| **AC2** Verdict/report 行 `'audit-provision-check: coverage: in-tx-ok'` / per-class counts；`tests/audit_provision.rs` 新解析器单测 | 报告第三行精确子串 `audit-provision-check: coverage: in-tx-ok`（干净态恒印；`format_lines` 单测 + 腿 C 步 4 grep）；per-class 行 `audit-provision-check: coverage-classes: admin=<a> message=<m> room=<r>`（Q9 GROUP BY class + `ClassCounts` 顺序填充）；新解析器 `parse_dedup_line`/`parse_class_counts` 单测（R8） | R4/R5/R8 |
| **AC3** harness 腿 C（throwaway DB）：seed 一条 `message.moderated` audit 行经 trigger 路径（binding 在、runtime 开）→ outbox 行同事务存在；binding 缺失 → 事务 abort（audit 行也缺席——原子性证据） | 腿 C 步 3-4（trigger 路径种子 + psql 1:1 断言 + `in-tx-ok` grep）与步 7（删 binding → INSERT 必须失败 + `COUNT(audit)==1` / `COUNT(outbox)==1` 零逃逸断言）；0239 缺席 → SKIP-with-reason | R6 |
| **AC4** 37/37 pin：新 slot 只经 `assert_b5_contract_pin` 加入；count==37 guard 绿 | `B5_CONTRACT_TEST_LIST` 换入 `audit-governance-coverage`（替换 `contract-test-22[PROPOSED]`，总数仍 37）；`assert_b5_contract_pin` 代码零改动；`test-b5-pin-guard.sh` :64/:113 期望串同步 → 自测绿；腿 C 发 `B5-CHECK audit-governance-coverage: PASS` 证据行 | R7 |

## 6. Coordination & hard rules（AGENTS §4）

- **命令面零扩张**：基础 `run()` 内增量；`--priority`、`network relay-probe`、`gate b5` 面不动；main.rs help/cmds 串不动。
- **fail-closed 窗口语义**：缺失探针只在 relay 开 + enabled-binding 作用域 fail-closed（A2 half 4 + 0241 mapped-subset 双重钉死）；关窗印 `fail-open-window` 信息行——**不许**为「acceptance 字面」牺牲部署库不假红（§1.1-B 是 direction problem 自己的目标）。
- **orphan 公式落 audit 侧**：`COUNT != COUNT(DISTINCT)` 按 §1.1-A 落 Q7（outbox 侧字面实现 = 死检，测试空转——违反「make it testable」）。
- **孤儿留存窗**：Q8 收窄于 `AERO__SERVER__AUDIT_RETENTION_DAYS`（默认 365 / 0=无界），窗外孤儿只报不红（§1.1-C）。
- **字面量纪律**：`admin.content.flag` 零出现（truth-check AUDIT-FLAG）；`message.moderated` 仅 SQL const；class 名经 `AuditClass::as_str()`。
- **尺寸纪律**：新代码全落 `audit_coverage.rs`（≤160 行）；`audit_provision.rs` delta ≤20 行（超 800 WARN 属软信号，HARD 1200 不触即 gate 绿；禁止顺手拆分既有代码）。
- **aero-eng 零 DB 依赖**：探针走 PsqlRunner（psql 子进程）；新模块不链接 connector/aero-ai/aero-storage；词表经 aero-common（`OutboxStatus` 导入先例已存在）。
- **活验证**：全新一次性库（CREATE → migrate → 用完 DROP；AGENTS §4.3）；腿 C 内建同一纪律；`make migrate-smoke` 不受影响（零迁移改动）。
- **并行集成**（若与 direction #1 同批）：两 spec 各新兄弟模块（`audit_class_report.rs` / `audit_coverage.rs`）、互不触碰；共享文件仅 `lib.rs`（两个 `pub mod` 行）与 `tests/audit_provision.rs`（各自追加）；`audit_provision.rs` 的 delta 面不重叠（direction #1 动 `G0239Counts`，本 spec 动 `AuditSnapshot`/`verdict`/`run` 的 coverage 分支）。`run()` 内探针顺序建议：Q6/Q7/Q8/Q9/Q10 在 Q5 之后、P 之前。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh`（0 违规）· `bash scripts/test-b5-pin-guard.sh`（换槽自测绿）。
