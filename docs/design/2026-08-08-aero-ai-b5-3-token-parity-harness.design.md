# Design — aero-ai B5-3：映射表 token 单源 + 37/37 具名 parity 清单落地（verify-and-hold + 三处收口）

- **Requirements**: `docs/requirements/2026-08-08-aero-ai-b5-3-token-parity-harness.req.md`（R1–R5 / P0）
- **Upstream designs**: `docs/design/2026-08-08-aero-common-b5-contract-vocabulary-leaf-types.design.md`（叶子单源 + AC4 守卫形态 §6 + F3 两处 harness 加固）；`docs/design/2026-08-07-aero-cli-b5-acceptance-gate-harness.design.md`（37/37 pin 骨架）；`docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md`（`--priority` CLI 面）
- **Landing owner**: `scripts/truth-check.sh` + `scripts/test-integration.sh`（**Rust 零改动**）；R1/R2/R3/R5 = verify-and-hold
- **Verification date**: 2026-08-08（全部锚点源码实读/实跑；行号会漂移，符号为准）

## 0. Evidence disposition（untrusted → verified）

requirements 文档的引用证据全部复核。除两处**已更正**（G2/G3）与两处**新增缺口**（G1、F2 计数漂移）外均属实：

| # | 证据主张（req spec） | 核对结果 | 锚点（实读） |
|---|---|---|---|
| E1 | proposal :10 并列两个 token | ✅ | `docs/proposals/audit-contract-batch-aero-im.md` B5-3 行：「本地 `message.moderated` → 出站 `admin.content.flag`/`admin.moderation.action` 映射表」 |
| E2 | 叶子锁定 const + pin 测试 | ✅ | `crates/aero-common/src/model/audit.rs`：doc :147-149（并列两 token + 锁定为一个常量）、`:150 pub const MODERATION_OUTBOUND_ACTION = "admin.content.flag"`、`:268-269 vocabulary_consts_are_pinned` 断言 |
| E3 | 0239:98,123 / 0241:110 SQL 字面量 | ✅ | 0239 :98 注释 + :123 `'action', 'admin.content.flag', -- MODERATION_OUTBOUND_ACTION (A2 half 5 field assertion)`；0241 :110 同值 INSERT |
| E4 | governance.rs re-export 链 + 6 测试 | ✅ | `crates/aero-ai/src/governance.rs` `pub use aero_common::model::audit::{…MODERATION_OUTBOUND_ACTION}`；`rg -c '#\[test\]'` = 6 |
| E5 | empty-filter guard + 0239-gated 块 | ✅ | `scripts/test-integration.sh` :196-202（`grep -Eq 'test result: ok\. [1-9][0-9]* passed'`，零匹配 exit 1）；:300-321 0239-gated 段（`audit_governance::` :309、`moderation_finalize_outbox_parity` :313，缺 0239 显式 SKIP） |
| E6 | implementation-gate.md :63/:78 | ✅ | :63 aero-im 行 1「30 个忽略测试 CI 全绿（37/37）；P2 parity」；:78 G6「37/37、T-11、moderation 优先级」 |
| E7 | 37 槽 = 15 executed + 22 [PROPOSED] | ✅ | `scripts/b5-pin.sh`：awk 精确计数 37 = 15 + 22（`contract-test-01..22[PROPOSED]`） |
| E8 | guard 自测全反例 | ✅ | `scripts/test-b5-pin-guard.sh`：count-36-fails / duplicate-slot-fails / malformed-slot-fails / vacuous-list-fails / SKIP_DB_CREATE 降级；`bash -n` OK；harness :148-149 在 DB 工作前先跑 |
| E9 | priority drill 三断言 + exit-2 能力门 | ✅ | `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`：`drill: moderation-in-first-batch: PASS` :243、`drain-501` :278、`parity-501` :296；:31-32 明确「0239 落地但 DESC 缺失 = FAIL 非 SKIP」；TRUNCATE-at-start 自隔离 |
| E10 | `aero-cli audit-provision-check --priority` | ✅ | `crates/aero-cli/src/main.rs` :489-501：`Some("--priority") => aero_eng::audit_provision::run_priority(&url)`（:497），未知 flag → usage error；`run_priority` :640；`priority_landed` :103、判定行 :192 |
| E11 | 0240 index == claim ORDER BY | ✅ | `migrations/0240_audit_governance_due_prio_idx.sql` `(priority DESC, available_at, created_at, event_id) WHERE status IN (0,1)` == `crates/aero-audit-connector/src/pg.rs:117` `ORDER BY candidate.priority DESC, …`（LIMIT pushdown） |
| E12 | T-11 drill + state_machine 回归 | ⚠️ **已更正（G3）** | drill ✅（`aero-audit-t11-drill.rs`：round 1/2 pending=N、terminal=0、`SUM(attempts)` 增长、transport-errors=N、`drill: t11-pending: PASS` :213）；**state_machine.rs 实为 7 个测试非 8**（`cargo test -p aero-audit-connector --test state_machine -- --list` 实测 = 7；`forbidden_dead_on_first_attempt` :242、`priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` :347 均在） |
| F1 | truth-check.sh 无 TOKEN 类别 | ✅ | `rg -n 'TOKEN|token|admin\.content\.flag|audit' scripts/truth-check.sh` = 0 命中；`rg 'admin.content.flag' crates/ --glob '*.rs'` = **4 处全在叶子** `audit.rs`（:11 doc、:147 doc、:150 const、:269 断言） |
| F2 | 全部未提交（87 项） | ⚠️ **计数漂移** | `git status --short` = **88 项**（并发 agent 工作树在动，计数只作参考；**具体文件清单**才是锚点——非 B5 untracked：`.pi-batch.lock`、`crates/aero-live-srt/src/isolation_tests.rs`、`docs/campaigns/audit-batch.out`、`examples/`） |
| — | `cargo check -p aero-common -p aero-ai -p aero-audit-connector -p aero-eng` | ✅ clean | 实跑 |

### 新发现（本 design 的收口对象）

| # | 事实 | 来源 | 影响 |
|---|---|---|---|
| G1 | **`run_migration_regression` 无 empty-filter guard**。5 个具名 migration-regression 槽（`rolling_upgrade_fences_are_atomic_before_0176_reasserts_them`、`migration_0192_…`、`migration_0228_…`、`migration_0233_…`、`migration_0237_…`）走 `run_migration_regression`（test-integration.sh :151-166），该函数直接跑 cargo test 不捕获输出——测试名改名/删除后槽位**静默空转绿** | vocabulary-leaf 设计 F3 项 ①「b5-pin 加固两处（本 design 纳入 step 7 前）」；实读 :151-166 | **W2**：给 `run_migration_regression` 补同款空过滤守卫——否则 R2「每个 executed 槽有命名过滤器」对 5 个迁移槽是空话 |
| G2 | **priority-drill 段 :482 无条件重复 PASS**。0239 存在分支尾部 `b5_check "moderation-priority-drill" "PASS"` 在 RC==2（SKIP）路径下与 :474 的 `SKIP (priority/class not landed)` **同时入账**——日志出现自相矛盾的双判词；RC==0 分支 :471 已写 PASS，:482 纯冗余 | vocabulary-leaf 设计 F3 项 ②；实读 :471/:474/:482 | **W3**：删除 :482（单路径单判词） |
| G3 | req R5.2「8 测试」不实 | `cargo test --list` 实测 | **D5**：验收改为「7 个测试全绿 + 具名清单」，计数仅核对参考（F8 同款教训） |

## 1. Design decisions

- **D1 — 本切片 = verify-and-hold + 三处收口**：R1（叶子单源）/ R2（37/37 pin）/ R3（priority drill）/ R5（T-11 + state_machine）**已全部落地**（§0 账本），验收走既有测试面，**零 Rust 改动**。本 design 的代码改动恰好三处，全部在 shell 侧：**W1**（R4 落地，req 的 F1 缺口——truth-check.sh TOKEN 类别）、**W2**（G1——`run_migration_regression` 空过滤守卫）、**W3**（G2——删重复 PASS）。W2/W3 是 vocabulary-leaf 设计 F3 明确要求「纳入本 design step 7 前」的加固，收进同一基线。
- **D2 — W1 形态 = vocabulary-leaf §6 原样**（AC4 守卫正确形态，防 `set -euo pipefail` 双向失效）：
  ```bash
  # 3. TOKEN 单源（AC4 回归守卫）：admin.content.flag 只允许出现在叶子 audit.rs
  token_leaks=$(rg -l 'admin.content.flag' crates/ --glob '*.rs' 2>/dev/null \
      | grep -v '^crates/aero-common/src/model/audit\.rs$' | wc -l | tr -d ' ' || true)
  if [ "${token_leaks:-0}" -gt 0 ]; then
      echo "  ❌ TOKEN LEAK: admin.content.flag 出现在叶子 model::audit 之外（单源违规）"
      orphan_violations=$((orphan_violations + 1))
  fi
  ```
  要点（全部 load-bearing）：① 尾部 `|| true` 吸收 rg 零命中的 exit 1（pipefail 下 naive 一行会误红）；② 过滤精确到**单文件** `crates/aero-common/src/model/audit.rs`（排除整个 `crates/aero-common/` 会漏同 crate 其他模块的字面量）；③ 范围仅 `crates/ *.rs`——migrations 0239:123/0241:110 与 docs 是 SQL/文档侧，SQL 无法 import Rust，由 db_tests 交叉 pin 覆盖（**不是守卫盲区，是设计边界**）；④ 递增既有 `orphan_violations` 计数（单一 exit 点 `exit "$orphan_violations"` 不变，TOKEN 违规 = 硬违规计入 exit 码）；⑤ 同步更新脚本头注释「检测项」与 summary 行（:202 增加 TOKEN 计数输出）。字符串拼接等技巧是 grep 固有盲区——语义兜底在叶子 `vocabulary_consts_are_pinned`（:268）。
- **D3 — W2 形态 = 照抄 `run_migrated_integration` 的 guard**（test-integration.sh :185-202）：`run_migration_regression` 改为捕获 `test_output`；cargo test 失败 → 打印输出 + `exit 1`；成功后 `grep -Eq 'test result: ok\. [1-9][0-9]* passed'` 零匹配 → 打印输出 + `exit 1`（"no test matched (empty-filter guard)"）；再 `drop_created_database` + `b5_check "${2}" "PASS"`。5 个迁移槽从此与 2 个集成槽同规。
- **D4 — W3 形态 = 删除单行**：test-integration.sh :482 `b5_check "moderation-priority-drill" "PASS"` 整行删除。判词矩阵变为：RC==0 → PASS（:471）、RC==2 → SKIP（:474）、其余 → FAIL（:476-479）、0239 缺 → SKIP（:484）。每个运行路径恰好一条判词，无矛盾证据。
- **D5 — 测试计数以实跑为准**：R5.2 的「8 测试」更正为 **7**（`cargo test --list` 为 ground truth）。验收不变式 = 「该文件全部测试绿 + 具名清单（7 个名字逐列）」，计数仅核对参考——套件增删测试时验收不因计数漂移误红（vocabulary-leaf F8 同款原则）。
- **D6 — P0 基线提交 = 显式路径**：`git add` 全部 B5 untracked（connector crate、`governance.rs`、`audit_provision.rs`、`audit_governance.rs`、drill bins、`migrations/0239-0241`、`b5-pin.sh`、`test-b5-pin-guard.sh`、`model/audit.rs`、本切片全部 req/design）+ 已修改 tracked 文件（root `Cargo.toml`/`.lock`、aero-ai lib/worker、aero-eng lib/run、aero-server、aero-storage lib.rs、`scripts/test-integration.sh`）。**勿 `git add -A`**——非 B5 untracked 存在（F2 清单）。提交后 `git status --short` 无 B5 文件残留（非 B5 文件不在检查范围）。

## 2. API changes

### 2.1 Rust API：零改动（已核证）

叶子 `MODERATION_OUTBOUND_ACTION` / `LOCAL_ACTION_MODERATED` / `GOVERNANCE_CLASS_*`、`aero_ai::governance::*` re-export 链、`GovernanceLane`、`audit_provision::{run,run_priority}`、drill bins、CLI `audit-provision-check [--priority]`、`PgOutboxRepo::claim_due` 签名与排序——**全部保持现状**。本切片不触碰任何 `.rs`。

### 2.2 Shell/CLI 行为变更（三处，全部 additive-or-stricter）

| 面 | 变更 | 兼容性方向 |
|---|---|---|
| `scripts/truth-check.sh` | 新增第 3 检测项「TOKEN 单源」硬违规（D2 形态）；summary 行增加 TOKEN 计数；exit 码语义不变（= 硬违规计数，现含 TOKEN） | **stricter**：现状扫描已干净（4 处全在叶子），当前库态全绿零行为差异；未来非叶子字面量 = 新红灯 |
| `scripts/test-integration.sh` `run_migration_regression` | 捕获输出 + empty-filter guard：命名过滤器零匹配 → exit 1（原静默绿） | **stricter**：5 个迁移槽的绿色门槛从「cargo test 退出 0」升为「≥1 测试实际跑过且 passed」 |
| `scripts/test-integration.sh` priority-drill 段 | 删除 :482 冗余 PASS | **纯修正**：SKIP 路径不再同时入账 PASS（判词矩阵见 D4） |

### 2.3 不变量（本切片验收后必须保持的对外契约）

- `B5_CONTRACT_TEST_LIST` = 恰好 37 槽（15 executed + 22 [PROPOSED]），`assert_b5_contract_pin` 的 count/dupe/malformed/vacuous/判词证据检查全绿。
- `B5-CHECK <name>: PASS|SKIP (<reason>)` 判词协议不变；每个 executed 槽每个运行路径恰好一条判词。
- `audit-provision-check` 三态退出码（0/2/other）与 `priority: landed|absent` 判定行不变。
- drill 能力门语义不变：0239 表/列缺 → exit 2 SKIP；**0239 落地但 DESC 缺失 → FAIL 红**。

## 3. Compatibility constraints

- **翻转协议（未来偶发事件）**：`admin.content.flag` 翻转 = 叶子 1 行（:150）+ 0239:123 + 0241:110 两处 SQL 联动编辑；db_tests（`audit_governance.rs` :320/:884 断言 `gov.4["action"] == MODERATION_OUTBOUND_ACTION`，导入叶子常量）是 DDL 侧回归网——单侧翻转必红。TOKEN 守卫**不**覆盖 SQL/docs 字面量（SQL 无法 import Rust），此为设计边界非盲区。
- **DESC 排序语义是 load-bearing**：`governance.rs` 头注释明确「与 `ai_job` 的 ASC lower-first 是**反向模型**，别"对齐"」——`GOVERNANCE_PRIORITY_MODERATION(100) > GOVERNANCE_PRIORITY_BACKLOG(10)` 下 moderation 先被 claim。任何「修复方向」都会反转车道并被 drill 红 + state_machine `priority_first_claim_preempts_fifo_…` 测试 + 0240 index 三面夹击。
- **37 槽构成固定**：15 executed + 22 [PROPOSED] 占位（仓外契约名，proposal :13/:15）；**不得**把占位替换为臆造名——契约文本落地时按 `b5-pin.sh` 头注释一键替换，guard 自动开始要求判词证据。
- **`set -euo pipefail` 双向失效**：truth-check.sh 是 `set -euo pipefail`——naive 一行 `rg … | grep -v …` 在零命中时 rg exit 1 误红、命中时管道 exit 0 漏报。D2 形态（`|| true` + 单一计数 + 单一 exit 点）是**唯一正确形态**，不得简化。
- **迁移编译期嵌入**：任何 throwaway 库流程 = `cargo build`（或等价 `cargo run --bin aero-cli -- migrate` 先编译）→ migrate。harness 已遵循；本切片零新增迁移。
- **零新依赖 / 零新迁移 / connector `src/` 零改动**：drill bins 已存在，本切片只动两个 shell 脚本。
- **工作树是活的**（并发 agent 在写）：证据快照以本 design 核验日期为准；提交前重跑 §6 验收（D6）。
- **str0m 声明位置**、`unsafe_code = "forbid"`、clippy 零新增警告等仓库级约束不受影响（无 Rust 改动）。

## 4. Failure modes

| # | 失效模式 | 触发 | 缓解（本 design） |
|---|---|---|---|
| F1 | 非叶子代码手写 `admin.content.flag` 字面量 | 新代码不引符号、直接拼字符串 | **W1** TOKEN 类别硬违规红 + exit 非零；叶子 pin 测试兜底拼接等 grep 盲区；SQL 侧由 db_tests 交叉 pin（设计边界） |
| F2 | TOKEN 守卫自身双向失效（零命中误红 / 命中漏报） | naive 一行在 `set -euo pipefail` 下 | D2 形态：`\| grep -v \| wc -l \| tr -d ' ' \|\| true` 吸收 rg exit 1；单一 `orphan_violations` 计数 + 单一 exit 点 |
| F3 | 迁移回归槽 vacuous green（测试名改名/删除后静默空转） | `run_migration_regression` 不查输出 | **W2**：同款 `test result: ok\. [1-9][0-9]* passed` 守卫，零匹配 exit 1 |
| F4 | priority-drill SKIP 路径出现矛盾双判词（SKIP + PASS 同入账） | :482 无条件 PASS 落在 RC==2 分支后 | **W3**：删 :482，单路径单判词 |
| F5 | 37 槽漂移（36/38、重复、malformed、全 [PROPOSED] vacuous） | 编辑清单失误 | 既有 `assert_b5_contract_pin` count/dupe/malformed/vacuous 检查 + `test-b5-pin-guard.sh` 自测（harness :148 先跑，DB 工作前即红） |
| F6 | DDL 翻转（0239/0241 字面量改）而 Rust 侧没跟 | 迁移编辑者只改 SQL | db_tests :320/:884 红（跨 pin 导入叶子常量）；TOKEN 守卫不适用 SQL（§3 边界） |
| F7 | DESC 排序回退（claim 改 FIFO） | 性能优化误改 `pg.rs:117` | drill `moderation-in-first-batch` FAIL 红（非 SKIP，:31-32 语义）；state_machine `priority_first_claim_preempts_…` 红；0240 index 与 ORDER BY 失配 |
| F8 | 基线丢失 / `git add -A` 污染 | 只提交本切片改动、或误收非 B5 文件 | P0（D6）：显式路径提交全部 B5 untracked + 在途 PR 另一半；`git status --short` 无 B5 残留检查；非 B5 文件清单列明 |
| F9 | 验收计数漂移（本切片已抓到一个：8→7） | 套件增删测试 | 验收以「命令 + 全绿 + 具名清单」为不变式；计数仅核对参考（D5） |
| F10 | 并发 agent 改动工作树导致核验结果过期 | 多 campaign 并行（docs/requirements 在核验期间持续新增文件） | 证据快照标注日期；提交前重跑 §6 全表 |
| F11 | drill 段 RC 捕获被 `set -e` 中断 | harness `set -e` 下直接跑 drill | 既有 `set +e` / `set -e` 包裹模式保留（:461-466 已如此），W3 不触碰该结构 |

## 5. Migration steps（rollout；**无 DB 迁移**——0239/0240/0241 零改动）

> 0239-0241 已在本切片基线内（§0 账本）。若 throwaway 库此前未 migrate，顺序仍是 **build → migrate**（AGENTS.md §4.2）。

0. **基线固化（P0）**：显式路径 `git add` 全部 B5 untracked + 在途 PR 另一半（D6 清单）；勿 `git add -A`。复跑 §6 全表作为基线快照。
1. **W1**：`scripts/truth-check.sh` 加第 3 检测项（D2 片段原样）+ 头注释「检测项」更新 + summary 行加 TOKEN 计数 → `bash -n scripts/truth-check.sh` + `scripts/truth-check.sh`（当前库态全绿）。
2. **W2**：`scripts/test-integration.sh` `run_migration_regression` 改捕获输出 + empty-filter guard（D3）→ `bash -n scripts/test-integration.sh`。
3. **W3**：删除 :482 重复 PASS（D4）→ `grep -n 'moderation-priority-drill' scripts/test-integration.sh` 应恰有 3 处（:471 PASS / :474 SKIP / :484 SKIP）。
4. **验证电池**（§6 全表）：`cargo check --workspace` 干净 · `cargo test -p aero-common --lib` · `cargo test -p aero-ai --lib` · `cargo test -p aero-audit-connector --test state_machine`（7）· `cargo clippy --workspace --all-targets` 零新增警告 · `bash scripts/test-b5-pin-guard.sh` · `SKIP_DB_CREATE=1 bash scripts/test-integration.sh` 干跑 pin 判词 · `scripts/{truth-check,file-size-check,web-check}.sh`。
5. **全链**：`scripts/test-integration.sh` 全跑（throwaway 库；`audit_governance::` / `moderation_finalize_outbox_parity` / `a3-relay-drill` / `t11-fail-closed` / `moderation-priority-drill` / `notification-fanout` / `relay-mock-probe` / `audit-provision-check` 判词齐全，relay 覆盖 legs ≥1）。
6. **收尾提交**：本 design + req 文档 + W1-W3 改动；`git status --short` 无 B5 残留。

## 6. Testable acceptance mapping（direction (a)–(d) + R1–R5 → 可执行检查）

| 验收 | 可执行检查 | 基线（2026-08-08 实跑/实测） |
|---|---|---|
| (a) 裁决记录 + 翻转 = 1 叶子 + 2 SQL 联动，跨 pin 跟随 → **R1 + R4** | `rg -n 'MODERATION_OUTBOUND_ACTION' crates/aero-common/src/model/audit.rs`（:150 const + :268-269 pin + :147-149 裁决注释）；`rg 'admin.content.flag' crates/ --glob '*.rs'` = 4 处全在叶子（排除叶子 = 0）；`cargo test -p aero-common --lib`；`cargo test -p aero-ai --lib`（governance 6 测试）；throwaway 迁移库 `cargo test -p aero-storage --lib audit_governance:: -- --ignored --test-threads=1`（:320/:884 跨 pin 绿）；**W1 负例**：临时在非叶子 crate 写字面量 → truth-check 红，删除恢复绿 | 4/4 命中叶子；两 lib 测试绿；db_tests 待迁移库实跑 |
| (b) 37/37 具名 + 命名过滤器 + 零匹配即红 → **R2** | `bash scripts/test-b5-pin-guard.sh` 全反例绿（count-36 / duplicate / malformed / vacuous / SKIP_DB_CREATE）；`SKIP_DB_CREATE=1 bash scripts/test-integration.sh` → `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`，改 36/38 槽 → FAIL；**W2 负例**：临时把某迁移槽过滤器改成不存在的名字 → harness 红（empty-filter guard），还原绿；`audit_governance::` / `moderation_finalize_outbox_parity` 各匹配 ≥1 现存测试（`audit_governance.rs` :248 同名 db_test） | 37 = 15 + 22（awk 实测）；guard 自测反例全在 |
| (c) priority drill：flag 行先于 prio-10 积压（0240 DESC）→ **R3** | harness moderation-priority-drill 段全绿：exit 0 + `priority: landed` 判定行 + `B5-CHECK moderation-priority-drill: PASS`（:471）；drill 三 PASS 行可见：`moderation-in-first-batch`（轮 1 `COUNT(status=2)==100` 且 moderation 行 ∈ 集——成员资格 = DESC 契约，非 `delivered_at == MIN`）、`drain-501`、`parity-501`；能力门：0239 表/`priority`/`class` 列缺 → exit 2 SKIP（:474）；**DESC 缺失 → FAIL 红**（不 SKIP）；**W3 验收**：SKIP 路径日志无矛盾 PASS 行 | drill bin :243/:278/:296 三断言；0240 index == pg.rs:117 ORDER BY（已核证） |
| (d) T-11 drill + state_machine 回归 → **R5** | harness t11-fail-closed 段全绿（:436 PASS；relay 缺席 → 行保持 pending、`SUM(attempts)` 增长、零终态、transport-errors=N、配给 seam 拒发 grant）；`cargo test -p aero-audit-connector --test state_machine` **7 测试全绿**（具名：`stale_token_cannot_ack_after_reclaim`、`backoff_is_bounded_and_exponential`、`permanent_error_dead_after_exactly_two_attempts`、`forbidden_dead_on_first_attempt`、`happy_path_settles_and_removes_from_claimable`、`skew_gt_lease_cannot_livelock_claim_fence_settle`、`priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`）；relay 覆盖 legs ≥1（`grep -q "^B5-CHECK …: PASS$" "$B5_LOG"` 计数模式 :581-582） | 7 tests（`--list` 实测；**req R5.2 的「8」已更正为 7**） |
| W1/W2/W3 自身 | 见 (a)/(b)/(c) 各负例 + `bash -n` 两脚本 | — |

## 7. Out of scope（红线复述）

- **不做契约翻转**：裁决已锁定 `admin.content.flag`；本切片只保证「翻转 = 叶子 1 行 + 0239:123/0241:110 两处 SQL 联动」的编辑协议与回归网（R1/R4），不执行翻转。
- **不实现 B5-3 排序本身**：`pg.rs:117` DESC claim 已落地（F3 事实），drill 是断言方；anti-starvation K-floor cap 是 sibling（claim-lane-carriage D7）已接受的 latent 边界，本切片不实现。
- **不改 37 槽构成**：15 executed + 22 [PROPOSED] 保持；仓外契约名不臆造（F4 事实）。
- **不实现 B5-4 auth 配给门 / B5-2 relay 本体 / L1 聚合**：sibling 切片职责；`audit-provision-check` 判词接线是既有 seam，本切片不动其语义。
- **不动 0239/0240/0241 DDL**：SQL 字面量是 DB 侧单源，仅经 db_tests 交叉互钉。
- **零新增迁移、零新增第三方依赖、connector `src/` 零改动、Rust 零改动**：本切片只改 `scripts/truth-check.sh` 与 `scripts/test-integration.sh`。
- **`truth-check` 的 participant_cache/notification_bundle 已知红项**：与本切片无关，不顺手修（脚本头注释 :30-33 已记录）。
