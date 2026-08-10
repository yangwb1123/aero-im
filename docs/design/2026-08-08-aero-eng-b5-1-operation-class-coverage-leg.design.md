# Design — aero-eng B5-1: audit-provision-check 操作类覆盖腿（Q6 class/action 分组 + L1 聚合证据）

- **落家**: `crates/aero-eng`（`audit_class_report.rs` 新兄弟模块 + `audit_provision.rs` 增量 + harness 腿 B3 + 单测）
- **Requirements**: `docs/requirements/2026-08-08-aero-eng-b5-1-operation-class-coverage-leg.req.md`
- **宿主命令**: `audit-provision-check`（`crates/aero-cli/src/main.rs:489-506` 命令臂，**本设计零改动**）
- **Sibling 设计**: `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`（命令本体）、`docs/design/2026-08-07-aero-cli-b5-acceptance-gate-harness.design.md`（leg 纪律）
- **Status**: Design（证据核验全部完成，见 §1；实现预算在 §2.4 精确到行）
- **Rev 2（本轮）**: 采纳 audit_integrity_reviewer 四项完整性修复（Q6 改 JSON framing、`class-malformed`/`class-untokenized` 可见行、absent=零行语义、coverage 交叉核对行）与 test_harness_reviewer 两项更正（§7 顺序断言、备选方案改 `PsqlRunner`+私有 `query` 方法为 `pub(crate)`）——见 §2.1/§2.4/§3/§5/§7 修订处。

## §1 证据核验（untrusted claims → 源码逐条对照）

| # | 声称 | 核验结果 |
|---|---|---|
| E1 | `audit_provision.rs` Q3/Q4/Q5_SQL + `parse_buckets` 全 status-only，零 class/action 分组 | ✅ 全中。Q3_SQL :49 `GROUP BY status`、Q4_SQL :52（仅 `status=0`）、Q5_SQL :55（仅 `status=3` LIMIT 5）；`parse_buckets` :300 经 `OutboxStatus::from_i32` fail-open（未知 status 忽略）；`format_report` 的 0239 行只有 `pending= claimed= delivered= dead=`；B5-3 的 `priority: landed\|absent` / `class: landed\|absent` 是 `information_schema` **列存在性**判定（P_SQL :62，多行 r-string 至 :68），非 class 行计数。缺口本体成立 |
| E2 | `audit.rs` 符号位置 + truth-check 锁 | ✅ 精确：`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` **:150**、`LOCAL_ACTION_MODERATED = "message.moderated"` **:156**、`/// High-volume message backlog class (L1-aggregatable).` **:165**、`GOVERNANCE_CLASS_MESSAGE` **:166**；`AuditClass` 枚举序 **Message→Room→Admin**（:119-122，serde lowercase，`const fn as_str()` :131-138）；truth-check AUDIT-FLAG mirror 在 `truth-check-lib.sh:277`（`CLAIM_AUDIT_FILE="crates/aero-common/src/model/audit.rs"` :126，rg 范围 `crates --glob '*.rs'`）——**Rust 新代码拼 `"admin.content.flag"` 即红** |
| E3 | 0239 有 class/action/priority 列 | ⚠️→✅ **更正成立**：`audit_governance_outbox` **无 `action` 列**——action 只存在于 JSONB `payload`（trigger `aero_enqueue_governance_audit` 的 `jsonb_build_object(…, 'action', 'admin.content.flag', …)`，16 键之一）；`class TEXT DEFAULT 'message' CHECK (class IN ('admin','message','room'))`、`priority SMALLINT DEFAULT 10 CHECK (priority > 0)`、`status CHECK (status IN (0,1,2,3))` 全中。**Q6 必须 `payload->>'action'`**，裸 `action` 列引用运行时报 `column "action" does not exist` |
| E4 | harness B1/B2 腿 + slot | ✅ `test-integration.sh`：leg B 注释 :253、B1（空库⇒consistent）:264-281、B2（1 条 v1 undelivered⇒fail-closed + `no audit:event:write grant issued`）:282-301、`drop_created_database` :302、`b5_check "audit-provision-check" "PASS"` :303、else-SKIP :305；`b5-pin.sh:44` = `audit-provision-check` slot（37/37 固定，**不新增 slot**） |
| E5 | 尺寸 790 行 + 拆分先例 | ✅ `wc -l` = 790；`file-size-check.sh` `MAX_LINES=800`/`HARD_LIMIT=1200`，比较符 `-gt`（**801 即 WARN**，790 只剩 **10 行**余量）；tests 头注释 "Kept out of the lib file to stay under the 800-line file-size WARN line" 即同款拆分先例 |
| E6 | `verdict()` 语义 | ✅ :134-155：dead-first fail-closed → relay 关 ∧ (v1 pending+claimed 或 0239 pending+claimed) > 0 ⇒ FailClosed（"no audit:event:write grant issued"）→ 全 0 ⇒ Consistent（exit 0）。**status=2 = delivered，永不进 undelivered 计数** ⇒ 腿 B3 种子必须 status=2 |
| E7 | action-token 词表 | ✅ `governance.rs`：`GOVERNANCE_PRIORITY_MODERATION=100` :31、`GOVERNANCE_PRIORITY_BACKLOG=10` :33、pass-through 测试集含 `message.create/edit/deleted`、`room.create/archived`（:124-131 附近）；`AuditClass::as_str()` 小写与 0239 CHECK 锁步（audit.rs 单测 :430-432） |
| E8 | :474 直插种子先例 | ✅ 精确同形：`INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload) VALUES (gen_random_uuid(), 2, 'admin', 100, jsonb_build_object('event_id', gen_random_uuid()::text, 'source_system', 'aero-im.source', 'action', 'admin.content.flag'))`——status=2 + payload 带 action 键 + 全 CHECK 合规；shell 字面量不触 truth-check（rg 限 `crates --glob '*.rs'`） |
| E9 | main.rs 零改动可行 | ✅ `cmds` 串 :316、命令臂 :489-506（`--priority` :497）、`run_priority` :665 起**先跑 `run()`**（base error 直接返回）——新行走 `run()` 即自动继承到 `--priority` 面；`tests/cli_smoke.rs` 存在（`--ignored` 门控 grep help 输出） |
| E10 | （本设计新增核对）`PsqlRunner` 可见性 | ✅ `struct PsqlRunner`（:437，**私有**）、`async fn query`（:466，**私有**）、`pub struct ConnParams`（:375，**已 pub**）——Q6 查询执行**必须留在 `audit_provision.rs` 的 `run()` 内**（或把 runner **和 `query` 方法**改 `pub(crate)`，§2.4 备选）；新模块只放纯函数 |
| E11 | （本设计新增核对）既有测试对报告行的断言兼容 | ✅ `tests/audit_provision.rs:312` 断言 `"audit-provision-check: class: landed"`、`:317` 断言 `"audit-provision-check: class: absent"`——均**全行前缀子串**；新 absent 行带类名（`class: admin: absent`）**不包含** `audit-provision-check: class: absent` 子串 ⇒ 不双命中；新计数行 `class-malformed: 0`/`class-untokenized: 0`/`class-coverage: …` 亦不含 `class: landed`/`class: absent` 子串。harness 现有零 `class:` grep ⇒ 零破坏面 |
| E12 | （Rev 2 新增）JSON framing 依赖 | ✅ `aero-eng/Cargo.toml` 已有 `serde_json.workspace = true`——Q6 改 `json_agg` + serde_json 解析**零新增依赖**；psql 恒以 `-At -v ON_ERROR_STOP=1` 单查询执行（:29 契约），JSON 输出单行无 `\n`/`|` 逃逸歧义 |
| E13 | （Rev 2 新增）词表校验通道 | ✅ `AuditClass` 派生 `serde::Deserialize` + `rename_all = "lowercase"`（audit.rs:119-122）——解析器可用 `serde_json::from_value::<AuditClass>` 做类名词表校验，**零裸字面量**（C5）且与 0239 CHECK 锁步（E7） |

**结论**：requirements 全部证据成立，事实更正（E3 无 action 列、E6 种子 status=2）与全部约束（E5 尺寸、E2 字面量锁、E9 main.rs 零改动、E10 可见性、E12/E13）复验通过。本设计在其上补实现级发现：**E10**（Q6 查询必须留在 run() 内，新模块只收纯函数）、**E11**（absent 行永远带类名）与 **E12/E13**（JSON framing 零新依赖、词表走 serde 通道）。

## §2 API 变更

### 2.1 新模块 `crates/aero-eng/src/audit_class_report.rs`（目标 ≤120 行）

```rust
//! B5-1 operation-class coverage leg (Q6): class/action grouping of the
//! 0239 governance outbox — the first CLI observation surface for L1
//! aggregation. Split out of audit_provision.rs (790/800 lines) — same
//! size-discipline precedent as the tests file header.
//!
//! JSON framing (Rev 2): psql `-At` does no cell escaping — a plain
//! `class|action|count` row stream lets a `\n`+`|` inside the action cell
//! break row framing and FABRICATE a phantom class row. `json_agg` +
//! serde_json closes that: every cell is a JSON string, `|`/`\n`/NULL are
//! unambiguous, and structurally broken elements are COUNTED (visible),
//! never vanished.

use aero_common::model::audit::AuditClass;
use serde_json::Value;
use std::fmt::Write as _;

/// Q6 — group by governance class and the wire action token inside the
/// JSONB payload (0239 has NO action column — the trigger stamps
/// `payload->>'action'`; a bare `action` reference fails at runtime).
/// No status filter: any status proves the in-tx write; dead rows still
/// fail the gate via the Q3 dead bucket. Single psql line (one JSON array,
/// COALESCE '[]' on zero groups); `{table}` is only ever one of the fixed
/// G0239_CANDIDATES literals (parse_probe_line-resolved). Subquery keeps
/// the (class, action) grouping and deterministic row order.
pub const Q6_SQL: &str = "SELECT COALESCE(json_agg(row_to_json(t)), '[]'::json) FROM (SELECT class, payload->>'action' AS action, count(*) AS count FROM {table} GROUP BY class, payload->>'action' ORDER BY class, 2) t";

/// Parsed Q6 evidence. INfallible by contract (§2.4): a Result-returning
/// parser would cost run() three more match lines and blow the 800-line
/// budget (790+12 → WARN). Every unreadable element lands in a visible
/// counter — nothing is silently dropped.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClassActionReport {
    /// Token-evidenced (class, action) groups, each with its row count.
    pub rows: Vec<(String, String, i64)>,
    /// Structurally malformed elements (non-object, count not i64, class
    /// outside the AuditClass vocabulary) — counted, never vanished.
    pub malformed: usize,
    /// Rows whose payload has no action token (NULL/empty action group
    /// counts summed) — counted, never rendered as `absent`.
    pub untokenized: usize,
}

/// Parse the `json_agg` array. Element-level failures bump `malformed`;
/// NULL/empty action groups bump `untokenized`. Array-level parse failure
/// (psql succeeded but output unparseable — unreachable given F1's
/// ON_ERROR_STOP) yields empty rows with `malformed = 1`, visible.
pub fn parse_class_action_counts(out: &str) -> ClassActionReport;

/// Greppable class-coverage lines, fixed order message → room → admin
/// (AuditClass declaration order — NEVER SQL row order). Present class →
/// exactly ONE aggregated line (L1 evidence: N rows fold into count=N);
/// contract class with ZERO rows → explicit absent line (absent means
/// zero rows, never "rows exist without tokens" — untokenized rows are
/// carried by the class-untokenized counter instead). Then the three
/// integrity lines: class-malformed / class-untokenized / class-coverage.
pub fn format_class_lines(r: &ClassActionReport) -> String;
```

- **输出行契约**（`format_class_lines`，前缀 `audit-provision-check: `，行序固定）：
  1. present：`audit-provision-check: class: <class> count=<N> actions=<去重后按行序 join ',' 的 token 列表>`；`<N>` = 该类各 action 组 count 之和（SQL 已按 (class, action) 分组，L1 聚合即单行）；
  2. absent：`audit-provision-check: class: <class>: absent`——**类名必带**（E11），且**仅当该类 Q6 输出零行**（Rev 2 语义，见 F4）；
  3. `audit-provision-check: class-malformed: <N>`——结构畸形元素计数（Rev 2）；
  4. `audit-provision-check: class-untokenized: <N>`——无 action token 行计数（Rev 2）；
  5. `audit-provision-check: class-coverage: covered=<N> total=<M>`——交叉核对行：`covered` = token 行总数，`total` = covered + untokenized + malformed（Rev 2：每条 outbox 行可对账，审计可比对 Q3 的 pending+claimed+delivered+dead 桶和）。
  - 类词表 100% 经 `AuditClass`（serde lowercase 反序列化校验 + `as_str()` 输出，E2/E7/E13），Rust 源码零裸字面量；action token 一律打印 DB 观察值。
- `pub` 面：`Q6_SQL` / `parse_class_action_counts` / `format_class_lines` / `ClassActionReport`（被 `audit_provision.rs` 与集成测试消费，integration tests 属外部 crate，`pub` 即 reachable，不触 `unreachable_pub`）；内部 helper（如 class 遍历序）私有。
- **不引入新依赖**：`serde_json` 已是 `aero-eng` workspace 依赖（E12）；无 async、无 DB 类型（E10：查询留在宿主）。

### 2.2 `crates/aero-eng/src/audit_provision.rs`（增量 ≤10 行，见 §2.4 预算）

1. `G0239Counts` 增字段（`dead_rows` :101 之后，同款 doc+field 两行式；全路径类型，**零 import 行**——import 是预算外行）：
   ```rust
   /// Operation-class coverage (B5-1 Q6): JSON-framed class|action|count — L1 evidence, any status.
   pub class_report: crate::audit_class_report::ClassActionReport,
   ```
2. `run()`（:552）：Q5 dead 块（:609-616）之后、`Some(G0239Counts {`（:617）构造之前追加（与 Q3/Q4/Q5 同款 `replace` + `outcome_error` 契约；**解析器 infallible**，故只 1 行调用）：
   ```rust
   let q6 = match runner.query(&crate::audit_class_report::Q6_SQL.replace("{table}", table)).await {
       Ok(o) => o,
       Err(e) => return outcome_error(e),
   };
   let class_report = crate::audit_class_report::parse_class_action_counts(&q6);
   ```
   构造字面量 `dead_rows,` 之后加 `class_report,` 一行。
3. `format_report` `Some(g)` 分支：dead_rows 循环（:190-192）之后、分支收尾之前：
   ```rust
   out.push_str(&crate::audit_class_report::format_class_lines(&g.class_report));
   ```
   （`push_str` 单行、全路径、零 import——预算关键；`format_class_lines` 返回自带换行的完整报告段。）
4. **零改动**：`verdict()`（Q6 数据不进 verdict 输入）、`run_priority()`（先跑 `run()` 天然继承）、P_SQL、Q0-Q5、`AuditSnapshot`。

### 2.3 `crates/aero-eng/src/lib.rs`

`pub mod audit_provision;`（:50）之后加一行 `pub mod audit_class_report;`（与现有 `pub mod` 块并列，:51 前插入）。

### 2.4 实现预算（精确到行，硬约束：audit_provision.rs 不得新增 WARN）

`file-size-check.sh` 比较符为 `-gt`（801 即 WARN）。当前 790 行 ⇒ 余量 **10 行**。本设计增量：

| 位置 | 行数 |
|---|---|
| `G0239Counts.class_report` 字段（单行 doc + field，全路径类型零 import） | +2 |
| `run()` Q6 查询+解析（4 行 match 块 + 1 行 infallible 解析调用）+ 构造字面量 `class_report,`（1 行） | +6 |
| `format_report` `push_str` 单行 | +1 |
| **合计** | **+9 ⇒ 799 ≤ 800** ✅（余 1 行） |

- **Rev 2 关键预算约束：解析器必须 infallible**（返回 `ClassActionReport` 而非 `Result`）。若 `parse_class_action_counts` 返回 `Result`，run() 需再加 3 行 match（`Ok/Err/};`）⇒ 增量 +12 ⇒ **802 > 800 → WARN**。数组级解析失败在 F1（psql 层）之后不可达，故 infallible + `malformed=1` 可见标记是正确取舍（F3）。
- **备选（若实现后仍超限）**：把查询+解析收进新模块 `pub(crate) async fn query_class_action_rows(runner: &PsqlRunner, table: &str) -> Result<ClassActionReport, String>`，**`PsqlRunner` 结构体（:437）与其私有 `async fn query` 方法（:466）均改 `pub(crate)`**（仅加关键字，不改行数）——`ConnParams` **已是 `pub`（:375），无需改动**（Rev 2 更正 harness-reviewer 反馈：原文本误列 ConnParams）。run() 增量降为 4 行。**首选仍是 §2.2 内联版**（runner 保持私有，爆炸半径最小）；备选仅在 `wc -l` 超 800 时启用。
- 新模块目标 ≤120 行（预算 ~100：头注释 8 + Q6_SQL 2 + struct 10 + parse 26 + format 36 + 杂项 ~10）；tests 文件 487 + ~110（7 处构造点 + R6 测试组含顺序/计数/absent 语义/ framing 断言）≤ 800 ✅。

### 2.5 报告行最终形态（新增 5 类，前缀统一）

```
audit-provision-check: class: message count=250 actions=message.create
audit-provision-check: class: room count=1 actions=room.create
audit-provision-check: class: admin count=1 actions=admin.content.flag
audit-provision-check: class-malformed: 0
audit-provision-check: class-untokenized: 0
audit-provision-check: class-coverage: covered=252 total=252
# 缺类时（该类 Q6 零行，Rev 2：仅零行渲染 absent）：
audit-provision-check: class: room: absent
```

- 与既有行并存：`class: landed|absent`（B5-3 列存在性）、`class: message count=`（新，行覆盖）——**grep 一律带类名或精确前缀**（§4 C4）。

## §3 Harness 腿 B3（`scripts/test-integration.sh`，复用现有 slot）

**插入锚点**：leg B 块内 `drop_created_database "$AUDIT_PROVISION_DB"`（:302）与 `b5_check "audit-provision-check" "PASS"`（:303）**之间**——B3 失败即 `drop + exit 1`，成功落到 :303 既有 PASS（不新增 b5_check 调用、不新增 slot，37/37 pin 不动）。

**前置**：`AUDIT_PROVISION_CLASS_DB="aero_audit_provision_class_$$"`（:38 变量区）+ `assert_disposable_db_name "audit provision class leg B database" "$AUDIT_PROVISION_CLASS_DB"`（:62 断言区，B1/B2 同款纪律；`^aero_[A-Za-z0-9_]{1,58}$` 合规）。

**流程**（B1/B2 同款 throwaway 纪律，AGENTS §4.3）：

1. `create_throwaway_database "$AUDIT_PROVISION_CLASS_DB"` → migrate（`cargo run --bin aero-cli -- migrate`）。
2. **种子（全 status=2——E6 铁律：status 0/1 会把 relay-off 库打成 fail-closed）**，`run_psql -v ON_ERROR_STOP=1` 三连：
   - message ×250：`INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload) SELECT gen_random_uuid(), 2, 'message', 10, jsonb_build_object('event_id', gen_random_uuid()::text, 'action', 'message.create') FROM generate_series(1, 250);`
   - room ×1：同款 `class='room'`、`priority=10`、`action='room.create'`（无 generate_series）。
   - admin ×1：同款 `class='admin'`、`priority=100`、`action='admin.content.flag'`（E8 先例形态）。
   - CHECK 合规：status 2 ∈ (0..3) ✓、class ✓、priority 10/100 > 0 ✓、delivery_mode 走 DEFAULT 'push' ✓、payload 为 jsonb object ✓、claim-state CHECK（无 claim_token/lease）✓。
3. 跑 `cargo run -p aero-cli -- audit-provision-check`，**非 vacuous 断言**（任一缺失 → 打印输出 + drop + exit 1）：
   - exit 0；
   - `verdict: consistent`（status=2 ⇒ undelivered=0 ⇒ consistent 路径）；
   - `class: message count=250 actions=message.create`（**L1 聚合单行**——250 行折一行；若实现退化成逐行打印，单测「每类恰一行」钉死，R6）；
   - `class: room count=1 actions=room.create`；
   - `class: admin count=1 actions=admin.content.flag`；
   - `class-malformed: 0`（Rev 2：畸形元素可见计数为 0）；
   - `class-untokenized: 0`（Rev 2：无 token 行可见计数为 0）；
   - `class-coverage: covered=252 total=252`（Rev 2：252 条种子全部对账）。
4. `drop_created_database "$AUDIT_PROVISION_CLASS_DB"` → 落 :303 既有 `b5_check "audit-provision-check" "PASS"`。

**门控**：沿用 leg B 现有 `grep -q "audit-provision-check"` help 检查（B3 在其 if 分支内，无新 gate）。

## §4 兼容性约束

- **C1 报告行纯增量**：既有行（relay/v1-outbox/outbox-0239/oldest-pending-age/dead/priority/class landed|absent/verdict）逐字节不变；新 class 行只在 `Some(g)` 分支追加。
- **C2 命令面零扩张**：main.rs、help、cmds 串、`--priority`、`network relay-probe`、`gate b5` 全不动；`run_priority` 经 `run()` 自动继承新行（零代码）。
- **C3 verdict 不变式 + 完整性可见性（Rev 2）**：`verdict()` 零改动；Q6 数据只进报告行，不进 verdict 输入；Q6 查询失败 = exit 1 fail-closed（与 Q3 同契约）。**行级问题（畸形/无 token）绝不静默**：一律进 `class-malformed`/`class-untokenized` 可见计数行，且 **exit 仍为 0**（计数行是审计可见信号，B3 断言 `= 0` 门控；退出码矩阵归 AC4 不动）。
- **C4 grep 精确性（双向防护）**：新行与 B5-3 `class: landed|absent` 并存。新 absent 行**永远带类名**（`class: admin: absent`），故既有单测 :312/:317 的全行子串断言不双命中（E11）；harness/文档 grep 用 `class: message count=` / `class: <c>: absent` / `class-malformed: 0` / `class-untokenized: 0` / `class-coverage: covered=` 精确模式，禁止裸 `class: absent` / `class: count=`。
- **C5 字面量纪律**：`"admin.content.flag"` 只允许存在于 audit.rs:150 与 shell 种子（truth-check rg 限 `crates --glob '*.rs'`，E2/E8）；新 Rust 代码零裸 class/action 字面量（词表经 `AuditClass` serde 反序列化校验 + `as_str()` 输出，token 打印 DB 观察值，E13）。
- **C6 无迁移、无新 slot**：0239/0240/0241 DDL、trigger、connector、storage 零改动；b5-pin 37/37 不变。
- **C7 表缺席行为不变**：Q2 探测无候选 ⇒ Q6 不运行、无 class 行（既有 `outbox-0239: not migrated` 覆盖）——pre-0239 部署输出与今日逐字节相同。
- **C8 尺寸门**：audit_provision.rs ≤800（§2.4 预算 799）、新模块 ≤120、tests ≤800（487+~110）、`file-size-check.sh` 零新增 WARN/HARD。
- **C9 既有测试兼容**：tests 7 处 `G0239Counts` 构造点（:96/:119/:152/:175/:204/:242/:298）机械补 `class_report: Default::default(),`（编译期强制）；verdict 测试零语义改动；cli_smoke 零改动（main.rs 无变化）必然绿。

## §5 失败模式

| # | 失败 | 行为 | 设计保证 |
|---|---|---|---|
| F1 | Q6 SQL 错误（表被并发 drop、列被改、psql 版本差异） | `ON_ERROR_STOP=1` → psql 非零退出 → `outcome_error` → **exit 1 fail-closed** | 与 Q3/Q4/Q5 同契约；门一旦决定跑 Q6 绝不静默跳过。数组级 JSON 解析失败同层处理：`malformed=1` 可见（F1 之后不可达，防御性兜底） |
| F2 | `{table}` 注入 | 只经 `parse_probe_line` 解析出的 `G0239_CANDIDATES` 固定字面量替换 | 与 Q3 同机制，非用户输入 |
| F3 | ~~行畸形被静默跳过~~ → **Rev 2 重写**：Q6 输出元素畸形（非 object / count 非 i64 / class 超词表） | 元素级容错：畸形元素计数入 `class-malformed`，**绝不消失、绝不误计数**；B3 断言 `class-malformed: 0` | JSON framing（`json_agg` + serde_json）：`|`/`\n`/NULL 均是无歧义的 JSON 字符串/字面量——**Rev 2 关闭了原 `splitn(3,'|')` 的 `\n`+`|` 幻影行伪造洞**（action 含 `\n`+`|` 时旧方案会裂行并伪造一条 class 行）；单测把该场景钉为「正确计数」而非「跳过」 |
| F4 | payload 无 `action` 键 / NULL / 空串 | `payload->>'action'` → NULL/空 → 该 group 计入 `class-untokenized`（Rev 2） | **absent 语义修正（Rev 2）**：absent 行仅当该类 Q6 **零行**；「有行但无 token」绝不再渲染成 absent（旧方案会伪装「写入静默丢失」门信号），而是 `class-untokenized: N` 可见 + coverage 行对账 |
| F5 | 表缺席（pre-0239 部署） | Q6 不运行，无 class 行 | C7：输出与今日逐字节相同 |
| F6 | 种子/真实数据 status 0/1 | verdict 保持 fail-closed（E6，未触碰） | 腿 B3 种子 status=2 规避；verdict 矩阵逐字节保留 |
| F7 | 报告行 grep 歧义 | 双命中风险 | C4：absent 行必带类名 + 精确 grep 模式；E11 已验证既有断言安全 |
| F8 | 尺寸超限（实现偏差） | `file-size-check.sh` WARN | §2.4 预算 799 + 备选 pub(crate) 方案回退（实现后 `wc -l` 复核） |
| F9 | psql 超时/网络黑洞 | `QUERY_TIMEOUT` 30s → 超时走 `outcome_error` | 既有 Q0-Q5 同款机制，Q6 复用 |

## §6 迁移步骤（落地顺序，每步独立可验）

> **无 DDL 迁移**——0239/0240/0241 已落地且零改动（C6）。「迁移」= 代码 + harness + 验证序列：

1. **新模块**：写 `crates/aero-eng/src/audit_class_report.rs`（Q6_SQL + `ClassActionReport` + `parse_class_action_counts` + `format_class_lines`，serde_json 已依赖，E12）。
2. **接线**：`lib.rs` 加 `pub mod audit_class_report;`；`audit_provision.rs` 按 §2.2 三处增量（字段 +2、run() +5、format +1）。**每步后 `wc -l crates/aero-eng/src/audit_provision.rs` 复核 ≤800**。
3. **单测**：`tests/audit_provision.rs` 7 处构造点补 `class_report: Default::default(),` + 新增 R6 测试组（§7：顺序断言、absent 语义、计数行、framing）。
4. **harness**：`test-integration.sh` 加 `AUDIT_PROVISION_CLASS_DB` 变量 + `assert_disposable_db_name` + 腿 B3 块（§3 锚点，含 `class-malformed: 0` / `class-untokenized: 0` / coverage 断言）。
5. **门禁**：`cargo check --workspace` → `cargo test --workspace --lib` → `cargo clippy --workspace --all-targets`（零新警告）→ `scripts/truth-check.sh`（新函数有调用，零 allowlist 新增）→ `scripts/file-size-check.sh`（零新 WARN/HARD）→ `scripts/web-check.sh`。
6. **活验证**：全新一次性库 `CREATE DATABASE` → `aero-cli migrate` → 腿 B3 种子三连 → `audit-provision-check` 断言三类行 + 三计数行 + exit 0 → `DROP DATABASE`（AGENTS §4.3；或直接跑 `test-integration.sh` leg B）。

## §7 可测试验收映射（requirements §5 AC 原句 → 断言）

| AC（原句） | 可测断言 | 测试形式 / 位置 |
|---|---|---|
| **AC1** Q6 `SELECT class, action, count(*) … GROUP BY class, action` + greppable 行 `class: message …`/`class: room …`/`class: admin …` 带 per-class action tokens | SQL 更正为 `payload->>'action'`（E3）且 **JSON framing（Rev 2）**：`Q6_SQL` 常量含 `json_agg(row_to_json(t))`、`payload->>'action' AS action`、`GROUP BY class, payload->>'action'` 片段（0239 无 action 列，裸列引用运行时炸）；`format_class_lines` 三类各一行、action join 按行序、去重；腿 B3 grep 三行全命中 | R1/R3 单测 + 腿 B3（§3.3） |
| **AC2** 种子 throwaway DB → 报三类 + exit 0；零行类给显式 `absent` 行而非沉默 | 腿 B3：252 行种子（250 message + 1 room + 1 admin，全 status=2）→ exit 0 + `verdict: consistent` + 三行 grep + `class-malformed: 0` + `class-untokenized: 0` + `class-coverage: covered=252 total=252`（非 vacuous：任一缺 → 腿 FAIL）；absent 语义单测（Rev 2）：仅 message 行 ⇒ `class: room: absent` + `class: admin: absent`；空 rows ⇒ 三 absent 行；**仅有未 token 行的类 ⇒ 不渲染 absent（`class-untokenized: N` 承载），absent 只表零行** | 腿 B3 + `format_class_lines` 单测 |
| **AC3** L1 证据：高量 backlog 行以 message 类聚合计数出现 | 250 条 message 种子 → 单行 `class: message count=250`（**每类恰一行**单测钉死：三类 rows → 恰 3 行输出，防退化成逐行打印）；**顺序断言（Rev 2，harness-reviewer 更正）**：`format_class_lines` 过滤 `class: ` 前缀行后逐行相等 `[message, room, admin]`（固定序，admin-first 实现即红）；词表锚点 `GOVERNANCE_CLASS_MESSAGE` "L1-aggregatable"（E2） | 腿 B3.3 + `format_class_lines` 行数与顺序断言 |
| **AC4** verdict 对 drain 健康（0/1/2/3 桶、dead-first fail-closed）不变——纯增量，绝不放松 | `verdict()` 零改动（git diff 验证）；Q6 数据不进 verdict 输入；既有 verdict 单测零语义改动（仅机械补 `class_report` 构造）；Q6 查询失败 → exit 1 fail-closed（F1 单测：`query` 返回 Err 路径经 `outcome_error`）；**计数行不改变退出码**（`malformed>0` 仍 exit 0，由行 + B3 门控——退出码矩阵 AC4 不动） | §2.2.4 + 既有测试套件 + 代码审查 |
| **AC5** 扩展 tests 解析器；cli_smoke 保持绿 | `parse_class_action_counts` 单测（Rev 2 重写）：常规多行组 / 空串与 `[]` → 空报告 / 畸形元素（非 object、count 非 i64、class 超词表）→ **入 `malformed` 计数（绝不消失）** / NULL 与空 action 组 → **入 `untokenized` 计数** / **action 含 `|` 与 `\n`+`|` → JSON 字符串无歧义，正确计数为单行（Rev 2：原「整行跳过」语义废除，改为钉死计数正确）**；`format_report` 集成：带 `class_report` 快照 → 含 class 行 + 三计数行；`g0239: None` → 无 class 行（既有 `report_not_migrated_has_no_age_or_dead_lines` 追加同款断言）；cli_smoke 零改动（main.rs 无变化）必然绿 | tests/audit_provision.rs 新增 ~110 行 |

## §8 范围红线

- **In**：Q6 查询+JSON 解析+报告行（基础 `run()`）；`G0239Counts.class_report`；`class-malformed`/`class-untokenized`/`class-coverage` 可见行（Rev 2）；腿 B3；新模块 `audit_class_report.rs`；单测。
- **Out**：`verdict()` 与退出码矩阵（零改动，AC4——计数行只做可见信号，不改 exit）；0239/0240/0241 DDL、trigger、connector claim SQL、storage 钻（in-tx 证明归 sibling，本腿只做观察面）；`--priority` 面（自动继承）；main.rs/help/cmds（零改动）；新 slot/新 gate（复用 `audit-provision-check`）；按 status 过滤 Q6（任何 status 都证明写入，dead 已由 Q3 单独 fail-closed）。
- **硬纪律**：audit_provision.rs ≤800（§2.4，解析器 infallible 是预算前提）；新 Rust 代码零裸字面量（C5，词表走 serde 通道 E13）；grep 带类名或精确前缀（C4）；腿 B3 种子全 status=2（E6）；活验证用全新一次性库（AGENTS §4.3）；提交前全部门禁过（§6.5）。
