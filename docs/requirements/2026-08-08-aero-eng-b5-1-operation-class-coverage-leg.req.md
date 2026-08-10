# Requirements Spec — aero-eng B5-1：audit-provision-check 操作类覆盖腿（Q6 class/action 分组 + L1 聚合证据，落家 = `crates/aero-eng`）

- **Module (analysis root)**: `crates/aero-eng` — `audit_provision.rs`（B5-4 psql-backed fail-closed 供给门 + B5-3 `--priority` drill 面）；本 direction 交付物 = 基础 `run()` 内的新 Q6 class/action 覆盖报告行 + harness 腿 B3 + 单测
- **Direction**: "Add B5-1 operation-class coverage leg to audit-provision-check (message.*/room.*/admin.* + L1-aggregation evidence)"（value 9 / risk_reduction 8 / effort 5 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-eng-36facc3d.json`（direction #1）
- **Campaign**: `aero-im-b5-outbox-relay`；gate anchor `docs/campaigns/implementation-gate.md`（G6 "37/37、T-11、moderation 优先级"）
- **Sibling specs（同模块/同批次，命令面协调）**: `2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（命令本体 = 本 direction 的扩展宿主）、`2026-08-08-aero-cli-b5-3-moderation-priority-drill.req.md`（`--priority` = `run()` 先行，天然继承新行）、`2026-08-08-aero-common-b5-contract-vocabulary-leaf-types.req.md`（class/action 词表单源）、`2026-08-08-aero-ai-b5-1-migration-0239-governance-outbox.req.md`、`2026-08-08-aero-ai-b5-3-token-parity-harness.req.md`
- **Status**: Requirements（下述证据全部经源码 grep 核对；**direction 两处事实性更正**——① 0239 表**无 `action` 列**，action 在 JSONB `payload` 内（`payload->>'action'`），Q6 原句 SQL 会运行时报 `column "action" does not exist`；② 0239 status 0/1 行已计入 `verdict()` 的 undelivered → 种子行必须 status=2 才能 exit 0。§1 逐条更正，acceptance 原句在 §5 保留并 re-ground）
- **Verification date**: 2026-08-08。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs`（Q3_SQL/Q4_SQL/Q5_SQL — status-only 聚合；`parse_buckets`） | ✅ 全中。Q3_SQL :49 `SELECT status, count(*) FROM {table} GROUP BY status ORDER BY status`；Q4_SQL :52（仅 `status = 0` 的最老 pending 年龄）；Q5_SQL :55（仅 `status = 3` 的 dead 明细，LIMIT 5）；`parse_buckets` :300 走 `OutboxStatus::from_i32`（fail-open，未知 status 忽略）。**全文件零 class/action 分组**——Q3 只按 status 桶，`format_report` :159 的 0239 行只有 `pending= claimed= delivered= dead=`，B5-3 的 `priority: landed\|absent` / `class: landed\|absent` :200-207 是**列存在性**判定，非 class 计数。direction 的缺口本体确认 |
| E2 | `crates/aero-common/src/model/audit.rs:20`（OutboxStatus 0..3）、`:156`（LOCAL_ACTION_MODERATED）、`:165`（L1-aggregatable backlog class）、`:168`（MODERATION_OUTBOUND_ACTION） | ✅ 符号全中，**行号轻微漂移**：`:20` `pub enum OutboxStatus`（显式判别值 Enqueued=0..Dead=3）精确；`:156` `LOCAL_ACTION_MODERATED = "message.moderated"` 精确；`:165` 是 doc 注释 `/// High-volume message backlog class (L1-aggregatable).`，常量 `GOVERNANCE_CLASS_MESSAGE` 在 **:166**；`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` 实际在 **:150**（非 :168）。「spelling locked to this file per truth-check.sh」✅ 属实：`scripts/truth-check-lib.sh:278`（AUDIT-FLAG mirror AC4）`rg -n -F '"admin.content.flag"' crates --glob '*.rs'` 硬失败于本文件之外的任何命中——**新 Rust 代码不得拼该字面量**（§4 R7） |
| E3 | `migrations/0239_audit_governance_outbox.sql`（class/action/priority columns, status CHECK 0..3） | ⚠️→✅ **一处事实更正**：`status INTEGER NOT NULL DEFAULT 0 CHECK (status IN (0,1,2,3))`、`class TEXT NOT NULL DEFAULT 'message' CHECK (class IN ('admin','message','room'))`、`priority SMALLINT NOT NULL DEFAULT 10 CHECK (priority > 0)`、`delivery_mode CHECK (delivery_mode IN ('push'))` 全中；trigger `aero_enqueue_governance_audit` 对 `NEW.action = 'message.moderated'` 打 `class='admin'`/`priority=100`/payload `'action', 'admin.content.flag'`。**但表无 `action` 列**——action 只存在于 JSONB `payload`（`jsonb_build_object` 16 键之一）；direction 声称的 "class/action/priority columns" 中 action 不成立。**Q6 必须 `payload->>'action'`**（§4 R1） |
| E4 | `scripts/test-integration.sh:246-281`（leg B1/B2 扩展点） | ✅ 位置 :250-291 语义保留：`AUDIT_PROVISION_DB` :38、leg B 注释 :250、B1（空库 ⇒ consistent）:264-281、B2（种子 1 条 v1 audit 行 ⇒ fail-closed + `no audit:event:write grant issued`）:282-291、`drop_created_database` + `b5_check "audit-provision-check" "PASS"`。37-slot 已含 `audit-provision-check`（`scripts/b5-pin.sh:44`）——**本 direction 不新增 slot**，腿 B3 进现有块 |
| E5 | （补充核对）`crates/aero-eng/src/audit_provision.rs` 行数 = **790**（`wc -l`）；`tests/audit_provision.rs` 头注释 "Kept out of the lib file to stay under the 800-line file-size WARN line" | ✅ `scripts/file-size-check.sh`：Rust `>800` WARN / `>1200` HARD（`exit $violations` 只数 HARD）。790 + Q6 面（SQL+解析+报告+接线 ≈ 45 行）必超 800 → **新生产代码放兄弟模块**（§4 R8，tests 头注释即尺寸驱动拆分先例） |
| E6 | （补充核对）`verdict()` 语义（audit_provision.rs :134-155） | ✅ 关键约束：relay 关 + **0239 pending+claimed > 0** ⇒ FailClosed（"no audit:event:write grant issued"）；relay 开 + dead=0 ⇒ Healthy（pending backlog 正常）。**种子行若 status=0 会把腿打成 fail-closed**（relay 关）→ 腿 B3 种子必须 status=2（delivered），verdict 走 `consistent` 路径 exit 0（§4 R5） |
| E7 | （补充核对）action-token 词表：`crates/aero-ai/src/governance.rs` :129-131 `"message.create"/"message.edit"/"message.deleted"`、:168 `"room.create"/"room.archived"`（pass-through 非 admin 类 token 测试集）；`GOVERNANCE_PRIORITY_BACKLOG=10` :33 | ✅ message 类 token 三个、room 类两个、admin 类 outbound token 一个（E2）；`AuditClass::as_str()` 小写拼写与 0239 CHECK 锁步（audit.rs:122/:133 + 单测 :399-432）。种子词表锚定这些 token（§4 R5） |
| E8 | （补充核对）`scripts/test-integration.sh:474` 已有直插 0239 种子先例（D8′ 负向检查） | ✅ `INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload) VALUES (gen_random_uuid(), 2, 'admin', 100, jsonb_build_object('event_id', …, 'action', 'admin.content.flag'))` ——status=2 + jsonb payload 带 action 键的完整先例；腿 B3 复用同形态（shell 内字面量不触 truth-check——rg 范围限 `crates --glob '*.rs'`，E2） |
| E9 | （补充核对）CLI 面零改动可行性与测试面 | ✅ `crates/aero-cli/src/main.rs`：命令臂 :489-506、help :489-490、cmds 串 :316——新行走基础 `run()`，**无新子命令/flag ⇒ main.rs 零改动**；`--priority`（run_priority :665-）先跑 `run()`，天然继承 class 行。`crates/aero-eng/tests/cli_smoke.rs`（`--ignored` 门控，grep help 输出）不受影响；`tests/audit_provision.rs`（487 行）是现有报告行解析器测试宿主（`report_contains_greppable_lines_*`、`report_priority_and_class_verdict_lines` 等） |

### 1.1 L1 聚合现状（direction acceptance 第三项 re-ground）

- **「无 repo evidence 任何 CLI 面观察 L1 聚合」成立**：全仓 grep 无 class 计数报告面（E1）；`GOVERNANCE_CLASS_MESSAGE` 的 "High-volume message backlog class (L1-aggregatable)"（E2）只存在于词表层与 0239 DEFAULT（`class TEXT NOT NULL DEFAULT 'message'`，E3），**无观察点**。本 direction 交付的 `class: message count=N`（单行聚合）即第一个 CLI 观察面。
- **诚实边界**：腿 B3 是**观察面证明**（种子直插 outbox → CLI 正确报告 class/action），**不是** in-tx 写入证明——in-tx 保证是 sibling storage 钻的领域（`aero-storage/src/audit_governance.rs` db_tests + `moderation_finalize_outbox_parity`，test-integration.sh 内 0239-gated 腿已 ACTIVE）。对真实库，class 行全 `absent` 即「写入静默丢失」的 gate 可见信号——这正是 problem statement 要堵的回归形态（§2 缺口 b）。

## 2. Verified current state

```
已存在（E1-E4，全部 verified）：
  0239 表（class/priority/status CHECK + token-keyed trigger）+ 词表单源（audit.rs leaf）
  Q3/Q4/Q5 status 桶 + dead-first fail-closed verdict + `audit-provision-check:` 前缀报告行
  B5-3 `priority: landed|absent` / `class: landed|absent` 列存在性行（--priority 面）
  harness 腿 B1/B2（throwaway DB + verdict grep）+ b5-pin.sh:44 slot

缺口（本 direction 关闭，全部 verified）：
  a) 门只按 status 桶看 drain 健康，从不按 class/action 断言 B5-1 核心声明——
     写路径回归（in-tx audit 写被静默丢弃 / L1 message 类聚合路径断）在 Q3 下全绿
  b) 无任何 CLI 面观察 L1 聚合（GOVERNANCE_CLASS_MESSAGE 无观察点）
  c) 0239 表在时无「三类 class 各有行」的可见性——只有列存在性（B5-3），无行覆盖
```

## 3. Scope

**In scope**：
- 基础 `run()` 内新增 Q6 class/action 分组查询 + 解析 + 报告行（R1-R3）
- `G0239Counts` 增 `class_rows` 字段（快照承载，R2）；`format_report` 增 class 行（含 absent 行，R3）
- harness 腿 B3（throwaway DB + 三类种子 + 非 vacuous grep，R5）
- `crates/aero-eng/tests/audit_provision.rs` 增解析器/报告行测试（R6）
- 新兄弟模块 `crates/aero-eng/src/audit_class_report.rs`（尺寸纪律，R8）

**Out of scope**：
- `verdict()` 语义与退出码矩阵——**零改动**（AC4：覆盖腿纯增量，绝不放松 fail-closed）
- 0239/0240/0241 DDL、trigger、connector claim SQL、storage 钻——零改动（in-tx 证明归 sibling 钻，§1.1）
- `--priority` 面（R1 的 `run_priority` 自动继承新行，无代码改动）
- main.rs / help 文本 / cmds 串——零改动（无新子命令，E9）
- 新 37-slot / 新 gate 门（复用现有 `audit-provision-check` slot，E4）
- 按 status 过滤 Q6（class 声明是「写入发生」，任何 status 都证明写入；dead 行已由 dead 桶单独 fail-closed，E6）

## 4. Requirements

### R1 — Q6 SQL（class/action 分组，action 从 JSONB payload 提取）

`crates/aero-eng/src/audit_class_report.rs`（新模块，R8）内：

```rust
/// Q6 — operation-class coverage (B5-1): group by governance class and the
/// wire action token inside the JSONB payload (0239 has NO action column —
/// the trigger stamps payload->>'action'; a bare `action` column reference
/// fails at runtime). No status filter: any status proves the in-tx write;
/// dead rows still fail the gate via the Q3 dead bucket.
const Q6_SQL: &str = "SELECT class, payload->>'action', count(*) FROM {table} GROUP BY class, payload->>'action' ORDER BY class, 2";
```

- `{table}` 只替换为 [`G0239_CANDIDATES`] 解析出的固定候选字面量（Q3/Q4/Q5 同款 `replace` 路径，`parse_probe_line` 解析，非用户输入）。
- **仅在 0239 表存在分支（`Some(table)`）内运行**，与 Q3/Q4/Q5 同生命周期；表缺席 → 不加 class 行（现有 `outbox-0239: not migrated` 行已覆盖，不印三个 `absent` 噪音行）。
- 查询失败 → 现有 fail-closed 路径（`outcome_error`，exit 1）——门一旦决定跑 Q6 就绝不静默跳过。
- 触发面：**基础 `run()`**（无 flag 路径）。`run_priority` 先跑 `run()` 自动继承新行，零改动。

### R2 — 解析器 + 快照承载

- `pub fn parse_class_action_counts(out: &str) -> Vec<(String, String, i64)>`——解析 psql `-At` 行 `class|action|count`；空行跳过；解析失败的行**忽略**（`parse_buckets` 同款 fail-open 扫描语义，Q6 输出在 ON_ERROR_STOP 下查询错误已前置失败，行级容错只针对数据）。
- `G0239Counts`（audit_provision.rs :91）增字段 `pub class_rows: Vec<(String, String, i64)>`——class 覆盖是 0239 表的属性，语义归属 G0239Counts（不放 AuditSnapshot 顶层）。`run()` 在 Q3 之后追加 Q6 查询与解析。
- 机械连带：`run()` 的 `G0239Counts` 构造 + `tests/audit_provision.rs` 现有 7 处 `G0239Counts` 构造点补 `class_rows: Vec::new()`（测试文件本就在扩展范围，R6）。

### R3 — 报告行契约（greppable，含显式 absent）

`format_report` 在 `Some(g)` 分支追加（**固定三类顺序 message → room → admin**，对齐 direction acceptance 原句顺序与 `AuditClass` 枚举声明序）：

```
audit-provision-check: class: message count=250 actions=message.create,message.edit
audit-provision-check: class: room count=1 actions=room.create
audit-provision-check: class: admin count=1 actions=admin.content.flag
```

- **present 行**：`audit-provision-check: class: <class> count=<N> actions=<去重后按出现序 join ',' 的 token 列表>`。`count` 是 Q6 GROUP BY 聚合值——L1 证据（250 行 message ⇒ 单行 `count=250`，绝不逐行打印）。
- **absent 行**：某合同类（message/room/admin，从 `AuditClass` 枚举 `as_str()` 派生，**禁止裸字面量**）零行 ⇒ `audit-provision-check: class: <class>: absent`——显式，非沉默（direction AC2 原句）。
- 报告打印**观察到的** action token（来自 DB `payload->>'action'`），Rust 源码**不拼** action 字面量（truth-check AUDIT-FLAG，E2/E7）。
- 表缺席 → 无 class 行（R1）。
- **grep 歧义防护**：现有 B5-3 行 `audit-provision-check: class: landed|absent`（列存在性）与新行 `class: <class>: absent` / `class: <class> count=` 并存。harness 与文档 grep 必须用**精确模式**（`class: message count=` / `class: admin: absent`），禁止裸 `class: absent`（会双命中）。
- 新增报告行放新模块的 `pub fn format_class_lines(rows: &[(String, String, i64)]) -> String`，由 `format_report` 调用（truth-check 零调用守卫：函数必须被调用，不加 allowlist）。

### R4 — verdict/退出码不变式（AC4）

- `verdict()` 零改动；fail-closed 矩阵（dead-first → relay 关 + undelivered → consistent/healthy）逐字节保留。
- Q6 数据**只进报告行**，不进 `verdict()` 输入。
- Q6 查询失败 = `Outcome::error`（exit 1，与 Q3 同契约）——覆盖腿自身也 fail-closed。

### R5 — harness 腿 B3（`scripts/test-integration.sh`，现有 leg B 块内追加）

位置：B2 之后、`drop_created_database "$AUDIT_PROVISION_DB"` 之前**不可行**（B2 已把该库打成 fail-closed 态）→ **独立 throwaway 库**（`AUDIT_PROVISION_CLASS_DB`，B1/B2 同款 `create_throwaway_database`/migrate/`drop_created_database` 纪律，AGENTS §4.3）。门控沿用现有 `grep -q "audit-provision-check"` help 检查（不新增 slot，E4）。

1. 建库 + migrate（`cargo run --bin aero-cli -- migrate`，B1/B2 同款）。
2. 种子（复用 E8 直插形态；shell 内字面量不触 truth-check）：
   - **message ×250（L1 高量，聚合证据）**：`INSERT INTO audit_governance_outbox (event_id, status, class, priority, payload) SELECT gen_random_uuid(), 2, 'message', 10, jsonb_build_object('event_id', gen_random_uuid()::text, 'action', 'message.create') FROM generate_series(1, 250);`
   - **room ×1**：同款，`class='room'`、`priority=10`、`action='room.create'`。
   - **admin ×1**：同款，`class='admin'`、`priority=100`、`action='admin.content.flag'`（E8 先例字面量）。
   - 全部 `status=2`（delivered）——**必须**：status 0/1 会经 E6 把 verdict 打成 fail-closed（relay 关）；status=2 走 `consistent` 路径 exit 0，同时 class 声明（写入发生）不受 status 影响。
   - payload 满足 `CHECK (jsonb_typeof(payload) = 'object')`（jsonb_build_object）；claim-state CHECK 满足（无 claim_token/lease_expires_at）；priority>0 CHECK 满足（10/100）。
3. 跑 `cargo run -p aero-cli -- audit-provision-check`，**非 vacuous 断言**（任一 grep 缺 → 腿 FAIL + 打印输出 + drop DB + exit 1，B1/B2 同款）：
   - exit 0；
   - `verdict: consistent`（E6 路径）；
   - `class: message count=250 actions=message.create`（**L1 聚合单行**——250 行折叠为一行；若实现退化成逐行打印，count= 与行数同时爆炸，grep 仍命中但报告被 250 行淹没——R6 单测以「每类恰一行」钉死）；
   - `class: room count=1 actions=room.create`；
   - `class: admin count=1 actions=admin.content.flag`。
4. drop DB + `b5_check "audit-provision-check" "PASS"`（现有 slot 复用）。

### R6 — 单测（`crates/aero-eng/tests/audit_provision.rs` 扩展）

- `parse_class_action_counts`：常规多行、空串、缺 cell 行忽略、非数字 count 忽略、action 含 `|` 的行（`splitn(3, '|')` 语义——action token 是 wire 词表，不含 `|`，但解析器按 `splitn` 防御）。
- 报告行（`format_class_lines`）：
  - 三类全在 → 恰三行 `class: <c> count=… actions=…`，顺序 message → room → admin；**每类恰一行**（聚合钉死）。
  - 仅 message 行 → `class: room: absent` + `class: admin: absent` 显式行。
  - 空 rows → 三个 `absent` 行。
  - 同一 class 多 action → 单行 `actions=a,b` join。
- `format_report` 集成：`G0239Counts` 带 `class_rows` 的快照 → 报告含 class 行；`g0239: None` → 无 class 行（`report_not_migrated_has_no_age_or_dead_lines` 同款断言追加）。
- 现有 `G0239Counts` 构造点机械补 `class_rows`（R2）；verdict 测试**零语义改动**（AC4 的测试面证明）。
- cli_smoke（`--ignored`）：**零改动**（main.rs 无变化，E9）——acceptance「cli_smoke remains green」以「无触碰」保证。

### R7 — truth-check / 字面量纪律

- 新 Rust 代码**不拼** `"admin.content.flag"` / `"message.moderated"` / class 字面量——词表一律经 `aero_common::model::audit`（`AuditClass` / `MODERATION_OUTBOUND_ACTION` / `LOCAL_ACTION_MODERATED`，audit_provision.rs :16 已有 `OutboxStatus` 导入先例）；报告只打印 DB 观察值。
- 新函数全部被 `run()`/`format_report` 调用——truth-check 零调用守卫不加 allowlist 条目。
- 种子 SQL 的字面量在 `scripts/test-integration.sh`（shell），不在 `crates --glob '*.rs'` 扫描范围（E2/E8 已验证）。

### R8 — 尺寸纪律（文件拆分）

- `audit_provision.rs` 现 **790 行**（800 WARN / 1200 HARD，`scripts/file-size-check.sh`）——Q6 面（SQL+解析+报告+接线 ≈ 45 行）放不进去。
- 新生产代码落**兄弟模块** `crates/aero-eng/src/audit_class_report.rs`（`lib.rs` `pub mod audit_class_report;`，与 `pub mod audit_provision;` :50 并列）：Q6_SQL、`parse_class_action_counts`、`format_class_lines`、`ClassActionCount` 元组类型别名（如需）。
- 目标 ≤120 行；`unreachable_pub = "warn"`（root Cargo.toml）——被集成测试消费的项 `pub`，纯内部项私有。
- tests 文件（487 行）加 ~80 行仍远低于 800 WARN。

## 5. Testable acceptance mapping（direction acceptance 原句保留，re-ground 到当前仓态）

| AC（原句） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **AC1** 新 Q6 `SELECT class, action, count(*) … GROUP BY class, action` + greppable 行 `class: message …`/`class: room …`/`class: admin …` 带 per-class action tokens | **SQL 更正为 `payload->>'action'`**（0239 无 action 列，E3）：`SELECT class, payload->>'action', count(*) FROM {table} GROUP BY class, payload->>'action' ORDER BY class, 2`；报告行 `audit-provision-check: class: message count=N actions=…`（R3）。单测：`format_class_lines` 三类各一行、action join；harness 腿 B3 grep 三行全命中（R5.3） | R1/R3/R5/R6 |
| **AC2** 种子 throwaway DB（test-integration.sh 模式）→ 报三类 + exit 0；零行类给显式 `absent` 行而非沉默 | 腿 B3：三类种子（250 message + 1 room + 1 admin，全 status=2）→ exit 0 + `verdict: consistent` + 三行 grep（R5.3）；absent 路径单测钉死（仅 message 行 → `class: room: absent` + `class: admin: absent`，R6）——absent 走单测而非第二个 DB 腿（更廉，语义等价） | R5/R6 |
| **AC3** L1 证据：高量 backlog 行以 message 类聚合计数出现 | 腿 B3 种子 250 条 message 行 → 单行 `class: message count=250`（GROUP BY 聚合；逐行打印则 250 行，单测「每类恰一行」防退化）；词表锚点 `GOVERNANCE_CLASS_MESSAGE` "L1-aggregatable"（E2） | R5.2/R5.3/R6 |
| **AC4** verdict 对 drain 健康（0/1/2/3 桶、dead-first fail-closed）不变——纯增量，绝不放松 | `verdict()` 零改动（R4）；Q6 数据不进 verdict 输入；现有 verdict 单测零语义改动（只机械补 `class_rows` 构造）；Q6 查询失败 → exit 1 fail-closed（与 Q3 同契约） | R4/R6 |
| **AC5** 扩展 tests/audit_provision.rs 解析器；cli_smoke 保持绿 | 新解析器/报告行测试（R6）；cli_smoke 零改动 + main.rs 零改动 ⇒ 必然绿（E9）；`cargo test --workspace --lib` + `cargo clippy --workspace --all-targets`（无新警告）绿 | R6/R8 |

## 6. Coordination & hard rules（AGENTS §4）

- **命令面零扩张**：基础 `run()` 内增量；`--priority`、`network relay-probe`、`gate b5` 面全部不动；main.rs help/cmds 串不动（E9）。
- **种子 status 纪律**：腿 B3 全部 status=2——status 0/1 会把 relay-off 库打成 fail-closed（E6），与「exit 0 报三类」acceptance 冲突；status=2 同时满足 claim-state CHECK。
- **字面量纪律**：`"admin.content.flag"` 只许出现在 audit.rs:150（truth-check AUDIT-FLAG mirror，E2）与 shell 种子（E8）；Rust 新代码经 leaf 常量/枚举，不拼字面量。
- **尺寸纪律**：`audit_provision.rs` ≤800 行（现 790）——新生产代码落 `audit_class_report.rs`（R8）；tests 文件 ≤800。
- **grep 精确性**：新行与 B5-3 `class: landed|absent` 并存——harness/文档 grep 用 `class: <class> count=` / `class: <class>: absent` 精确模式（R3）。
- **aero-eng 零 DB 依赖**：Q6 走 PsqlRunner（psql 子进程，现有先例）；新模块不链接 connector/aero-ai/aero-storage；词表经 aero-common（已有 `OutboxStatus` 导入先例，E2）。
- **活验证**：全新一次性库（`CREATE DATABASE` 再 `aero-cli migrate`，用完 `DROP DATABASE`；AGENTS §4.3）；腿 B3 内建同一纪律。
- **迁移纪律**：本 direction 不加迁移；0239/0240/0241 已落地，勿改。
- **提交前必过**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/truth-check.sh`（新函数有调用，零 allowlist 新增）· `scripts/{file-size-check,web-check}.sh`（无新 HARD；`audit_provision.rs` 不得新增 WARN）。
