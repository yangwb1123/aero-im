# API Review — aero-eng B5-1 §2 concrete Rust（实现前审查）

- **对象**: `docs/design/2026-08-08-aero-eng-b5-1-in-tx-coverage-probe.design.md` §2（`audit_coverage.rs` + `audit_provision.rs` delta）
- **方法**: 源码逐条对照（`audit_provision.rs` 790 行 / `tests/audit_provision.rs` 487 行 / `outcome.rs` / `retention.rs:24` / `audit.rs:133-168` / 0239 DDL）+ **活库 psql 实证**（throwaway 库已 drop）+ 完整实现草稿行数实测
- **结论**: 5 项中 2 项通过、3 项需修（其中 1 处是**编译不过的接线缺陷**）；实现预算两处不成立

## 审查项 1 — fail-closed 错误传播：**通过（1 个规格缺口）**

**活库实证**（aero-postgres throwaway 库，`psql -At -v ON_ERROR_STOP=1`）：

| 实验 | 结果 | 对设计的意义 |
|---|---|---|
| 多语句 RR RO 事务 | 输出**含 `BEGIN`/`COMMIT` tag 行**（`BEGIN\nQ6\|3\nQ7\|t\|2\nQ9\|admin\|1\nCOMMIT`，exit 0） | 路由层跳过 `BEGIN`/`COMMIT` 是**必需逻辑，非死代码**（E11 说「-q 可压」——设计选跳过，正确；`PsqlRunner` 零改动成立） |
| 语句中途 `1/0` | exit 1；stdout 有**部分输出**（`BEGIN\nQ6\|1`）；stderr 有 `ERROR: division by zero` | `PsqlRunner::query`（:466）先查 `output.status.success()`——exit≠0 → Err（stderr 文本），**部分 stdout 永不进解析器**。psql 错误文本 vs tag 行**结构性分离**：stderr=query Err 路径、stdout=仅 exit 0 才解析 |
| Q9 GROUP BY 空表 | **该节零行**，Q10 仍产 `Q10\|0` | 「Q9 可零行」正确且可区分（Q10 是锚）；注意 E11 的「聚合语句恒产一行」措辞对 Q9 不精确（GROUP BY 空表零行）——设计 §2.1 解析器契约是对的，证据行措辞松 |
| 空输入 COUNT(*) 无 GROUP BY | 恒一行 `Q6\|0` | Q6/Q7/Q8/Q10「恰一行」结构性成立 |

**Err 路径 → exit 1 全链**（`Outcome::error` 实测 exit_code=1，outcome.rs:62）：query Err → `probe` `?` → Err → run() `outcome_error`（:554 闭包在 `Some(table)` 臂作用域内 ✓）→ exit 1；parse Err（缺节/超行/未知 tag/坏 cell）→ 同链 exit 1；Q9 零行 → fail-open（F3 设计态）。✓ 全部映射到文档化 exit code。

**缺口**：`parse_probe_output` 契约不拒**乱序** Qx 节（tag 路由天然序无关；psql 单 `-c` 输出确定性有序，乱序=probe_snapshot_sql 编程错误）。审查项明确要求乱序→Err。修法：路由层记单调递增节序（`Q6<Q7<Q8<Q9<Q10`，出现更小 tag → Err）+ 单测。或显式文档声明「序无关是有意的」——二选一，不能留白。

## 审查项 2 — 解析器契约不变：**通过**

- `parse_dedup_line(line)` 收 tag 剥离后的单行 `f|162000`：双 cell、bool→i64、缺 cell/垃圾 → Err——与 `parse_priority_probe`（:235）/`parse_relay_line`（:270）同构，契约不变 ✓。`parse_psql_bool_line`（:225，pub）可跨模块复用（`use crate::audit_provision::parse_psql_bool_line;`——同 crate 兄弟模块引用合法，单向依赖 audit_coverage→audit_provision）。
- `parse_class_counts(out)` 收整节：跳过空行/坏行/未知 class——`parse_buckets`（:300）fail-open 哲学 ✓。class 列被 0239 CHECK 锁死 `admin|message|room`（E10 已核）→ 无 `|`/换行注入面，`split('|')` 安全 ✓。
- 路由层对 Q6/Q8/Q10 单 cell 直接 `parse::<i64>`、Q7 剥 tag 后委派 `parse_dedup_line`、Q9 收集后委派 `parse_class_counts`——两个被委派解析器收**无 tag 行**，tag 只在路由层 ✓。

## 审查项 3 — `audit_retention_window_days` clamp + 纯函数：**不通过（需修）**

- **clamp 位置/数值正确**：`clamp(0, 365_000)` 在 env 边界一次收口；365_000 < int32 days（2,147,483,647，F17 实测超界报错）且 < timestamptz 安全天花板（≈245 万天）✓。`q8_orphan_sql`/`probe_snapshot_sql` 只从该函数取值，生产路径无绕过 ✓。
- **纯函数设计未落地**：`pub fn audit_retention_window_days() -> i64` 直接读 env——不是纯函数。§6 单测清单只测带显式 `days` 参数的纯 builder，**default/invalid/negative/clamp 逻辑零测试**。testing_reviewer F9 的「纯 `parse_retention_days` 免并行 env 竞态」修正**未采纳**（设计声称已处理，实为「不测」）。若有人补 F9 测试用 `set_var`，与并行测试线程竞态（本测试文件现无任何 env 变异先例，全纯函数风格——保持）。
- **修法**（+1 函数、+7 断言，零 env 变异）：
  ```rust
  fn parse_retention_days(raw: Option<&str>) -> i64 {
      raw.and_then(|s| s.parse::<i64>().ok()).unwrap_or(365).clamp(0, 365_000)
  }
  pub fn audit_retention_window_days() -> i64 {
      parse_retention_days(std::env::var("AERO__SERVER__AUDIT_RETENTION_DAYS").ok().as_deref())
  }
  ```
  单测：`None→365` / `Some("abc")→365` / `Some("-5")→0` / `Some("0")→0` / `Some("365")→365` / `Some("365000")→365000` / `Some("999999999999")→365000`。

## 审查项 4 — 行预算：**不通过（两处均破）**

### 4a. `PsqlRunner` delta = 恰 2 行 ✓；但 §2.2 接线有**作用域缺陷**

- `PsqlRunner` 私有部分确认：`struct` :437、`query` :466 均 private，仅两处 `pub(crate)` 关键字即够（实证：单 `-c` 多语句 + 现有参数全兼容，`-q` 不需要，超时/错误路径零改动）✓。
- **编译缺陷**：§2.2 项 4 把 `let coverage = match probe(...)` 插进 `Some(table)` **臂内**（:585-619），但项 5 的快照构造（:629-635）在 match **之外**——`coverage` 臂内作用域，函数级不可见 → **编译不过**。修法二选一：
  - **A**（最小）：match 前 `let mut coverage = None;`（+1 行），臂内 `coverage = match probe(&runner, table).await { Ok(c) => Some(c), Err(e) => return outcome_error(e) };`（+4 行）
  - **B**：把 `let table = parse_probe_line(&q2);` 提升到 match 前（改 1 行），match 后 `let coverage = match table { Some(t) => match probe(&runner, t).await {...}, None => None };`（+6 行）
- **行数实算**：§2.2 五项 = 1+4+3+4+1 = **13 新增**（「≤13」只在纯新增口径下成立）；加 2 处 `pub(crate)` 修改 = **15 触及**；作用域修复后 14-15 新增 + 2-3 修改 = **16-18 触及**。「含 2 处 pub(crate) ≤13」在任何口径下都不成立，须改述为「≤15 新增 + 2 关键字修改」。

### 4b. `audit_coverage.rs` ≤190 行预算：**不成立（实测 ~276）**

设计 §2.1 骨架实测 **132 行**，其中 8 个函数是 `;` 存根。按代码库既有风格（对照 `parse_relay_line` 17 行 / `parse_buckets` 24 行 / `parse_v1_line` 29 行）补齐全部函数体并保持文档密度的**完整实现草稿 = 276 行**（`wc -l` 实测），明细：`parse_probe_output` 64 / `format_lines` 23 / `parse_dedup_line` 20 / `parse_class_counts` 17 / `q8_orphan_sql` 17 / `breach_components` 16 / `retention` 15 / 文档 ~45。激进压缩文档也难低于 ~240。**≤190 与所述 API 面（8 函数+2 struct+5 const+2 builder+路由+probe）不可兼得**——须改述为 ≤290（仍远低于 800 WARN，模块拆分目的「audit_provision.rs 守 800」不受影响），或砍 API（并 `parse_class_counts` 进路由、去 `ClassCounts` struct——但伤报告/测试面，不建议）。

附带：`tests/audit_provision.rs` 追加 ~90 也低估——§6 清单实估 ~145-160 + 既有 `AuditSnapshot` 构造机械补 `coverage: None`（实测 6 处字面构造 :70/:88/:115/:196/:238/:290 + `snapshot()` helper :8，helper 一处改、字面六处各加一行）≈ 总计 160-180；487+180 ≈ 667 < 800 WARN ✓ 无硬违规，但预算数字要改。

## 审查项 5 — 单测覆盖 + `breach_components` 不可分歧：**不通过（3 个缺口）**

**导出函数覆盖矩阵**（§6 清单 vs §2.1 导出面）：

| 导出项 | §6 覆盖 | 结论 |
|---|---|---|
| `parse_dedup_line` | 5 用例 | ✓ |
| `parse_class_counts` | 4 用例 | ✓ |
| `q8_orphan_sql` | days 365/0 | ✓ |
| `coverage_verdict` | 4×2 矩阵 | ✓ |
| `format_lines` | 干净/违约/关窗 | **部分**——缺 F6 交叉用例（见下） |
| `parse_probe_output` | 路由/缺节/超行/未知 tag | **部分**——缺乱序用例（见项 1） |
| `probe_snapshot_sql` | BEGIN/COMMIT/tag/table/days=0 | **部分**——缺 Q6 enabled-binding join 文本断言（testing_reviewer 的 F12/F14 修正承诺「U1-U5 具体添加」，最终 §6 未兑现：`probe_snapshot_sql` 测试只查「含五语句首列 tag」，不查 `LEFT JOIN snaplink_commercial_bindings b` + `b.workspace_id IS NOT NULL`） |
| `audit_retention_window_days` | **无** | **缺口**（见项 3） |
| `probe` | leg C e2e | ✓（与 run() 同理，不可单测） |
| `breach_components` | 经两消费者间接 | ✓ |

**`breach_components` 不可分歧性**：结构上今天成立——`coverage_verdict` 与 `format_lines` 第三行都消费同一派生。但**分歧面真实存在**：`format_lines` 的 fail-open-window 分支是独立谓词 `!relay_enabled && c.missing > 0 && parts.is_empty()`，必须与 `coverage_verdict` 的关窗 None 语义精确同构。推导验证：relay 关时 parts 不含 missing，`parts.is_empty()` ⟺ dedup=f ∧ orphans=0——谓词正确（relay 关 + orphans>0 → `parts=[orphan]` → 走 in-tx-broken ✓ F6；relay 关 + dedup=t → in-tx-broken ✓ F10）。同构关系应显式钉死为：**verdict None ⟺ 第三行 ∈ {in-tx-ok, fail-open-window}；verdict Some(r) ⟺ 第三行 = 同一组件串的 in-tx-broken**。但设计未文本钉死该谓词（format_lines 体未示），且 §6 单测**缺 relay 关 + missing>0 + orphans>0 的 format_lines 用例**——恰是 fail-open 谓词写错时唯一被抓的陷阱。补：format_lines × (关窗 / F6 交叉 / dedup+missing relay 关) 三用例。

## 附带发现（实现期易错点）

1. **`{table}` 替换陷阱**：`probe_snapshot_sql` 用 `format!` 插值 const 值时，const 内的 `{table}` **不会**被二次替换（format! 只解释格式串本身）——必须对含 `{table}` 的 const（Q6/Q8/Q9/Q10）逐一 `.replace("{table}", table)`，否则 psql 报 `syntax error at or near "{"` → 误 fail-closed。Q3/Q4/Q5 的 `run()` 内联 `replace` 是既有正确先例。
2. E11「聚合恒产一行」对 Q9 不精确（GROUP BY 空表零行）——契约文本已对，证据行措辞建议改。
3. F1 索引括注微偏：0239 唯一索引是 `audit_governance_due_idx (available_at, created_at, event_id) WHERE status IN (0,1)`（**partial**，:55-57）；Q6 反连接探的是 outbox PK `event_id`。无害，但表述建议对齐。
4. 模块互引方向：audit_provision → audit_coverage（verdict/format_report 调用）与 audit_coverage → audit_provision（`parse_psql_bool_line`、`PsqlRunner`）——同 crate 模块互引合法，无环。

## 结论

| 项 | 判定 | 必修 |
|---|---|---|
| 1 fail-closed 传播 | ✅ 通过 | 补乱序→Err（或显式声明序无关） |
| 2 解析器契约 | ✅ 通过 | — |
| 3 retention clamp | ❌ | 拆纯 `parse_retention_days(raw: Option<&str>)` + 7 断言（clamp 现零测试） |
| 4 预算 | ❌ | §2.2 作用域缺陷（编译不过，须提升 `coverage` 声明）；`audit_coverage.rs` 改述 ≤290（实测 276）；tests ~90 → ~170 |
| 5 单测覆盖/同构 | ❌ | 补 retention 用例、F6 format_lines 交叉用例、乱序用例、Q6 join 文本断言；同构关系显式钉死两态映射 |

设计主体（单快照 D9、tag 路由、契约、verdict 矩阵、exit 码映射）经源码 + 活库实证**全部成立**；上述为落地前必须合入的修正项，不推翻设计方向。
