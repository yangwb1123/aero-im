# Requirements Spec — B5-3 映射表 token 歧义消解 + 37/37 具名 parity 清单落地（aero-ai 切片）

- **Module (analysis root)**: `crates/aero-ai`（`governance.rs` 车道映射）+ 联动：`aero-common` 叶子词表、0239/0240/0241 迁移、`scripts/{test-integration.sh,b5-pin.sh,test-b5-pin-guard.sh,truth-check.sh}`、drill bins
- **Direction**: "Resolve B5-3 mapping-table token ambiguity (admin.content.flag vs admin.moderation.action) and land the 37/37 named parity list in the harness"（value 6 / risk_reduction 4 / effort 2 / confidence 9）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-f8cd3622.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（:10 映射表同时列出两个 token；:13 "37/37 测试清单不可独立核对"；:15 "37/37 需先把契约测试清单钉入 test-integration.sh"）；gate anchor `docs/campaigns/implementation-gate.md`（:63 aero-im 行 1 "30 个忽略测试 CI 全绿（37/37）"、:78 G6 = "37/37、T-11、moderation 优先级"）
- **Upstream designs**: `docs/design/2026-08-08-aero-common-b5-contract-vocabulary-leaf-types.design.md`（叶子单源 + AC4 TOKEN 守卫）；`docs/design/2026-08-07-aero-cli-b5-acceptance-gate-harness.design.md`（37/37 pin 骨架 + drill 断言，D1–D8 事实）；`docs/design/2026-08-07-aero-ai-b5-3-priority-claim-ordering.design.md`（priority DESC）
- **Status**: Requirements。证据核对日期 2026-08-08（全部实读/实跑，见 §1 账本）
- **行号约定**：行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `docs/proposals/audit-contract-batch-aero-im.md:10` 同时列出 `admin.content.flag` 与 `admin.moderation.action` | ✅ **Verified（:10 精确命中）**。"B5-3：`priority` 列 + `claim_due` 按 `priority DESC` 排序；本地 `message.moderated` → 出站 `admin.content.flag`/`admin.moderation.action` 映射表；注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限"。**两个 token 并列表述，无单一裁决**——这正是本 direction 要消解的歧义 |
| E2 | `crates/aero-common/src/model/audit.rs:147-150` 锁定 `MODERATION_OUTBOUND_ACTION = "admin.content.flag"`；:269 pin 测试 | ✅ **Verified（const 在 :150、pin 断言在 :269，注释 :147-149 记录裁决）**。`pub const MODERATION_OUTBOUND_ACTION: &str = "admin.content.flag";`（:150）前注释："The contract proposal listed both `admin.content.flag` and `admin.moderation.action`; locked here as ONE constant — a flip is a one-line edit and the cross-pins follow automatically."。`vocabulary_consts_are_pinned`（:268-279）断言 `MODERATION_OUTBOUND_ACTION == "admin.content.flag"`（:269） |
| E3 | `migrations/0239_audit_governance_outbox.sql:98,123` 与 `migrations/0241_governance_reconcile.sql:110` 硬编码同一字面量 | ✅ **Verified**。0239 :98 注释 `-- outbound_action: "admin.content.flag" /* MODERATION_OUTBOUND_ACTION */`；:123 触发器 INSERT `'action', 'admin.content.flag', -- MODERATION_OUTBOUND_ACTION (A2 half 5 field assertion)`；0241 :110 reconciler INSERT `'action', 'admin.content.flag',  -- MODERATION_OUTBOUND_ACTION`。**两处 SQL 字面量 = flip 时的 2 处联动编辑点** |
| E4 | `crates/aero-ai/src/governance.rs` re-export 链 | ✅ **Verified**。`pub use aero_common::model::audit::{GOVERNANCE_CLASS_ADMIN, GOVERNANCE_CLASS_MESSAGE, GOVERNANCE_CLASS_ROOM, LOCAL_ACTION_MODERATED, MODERATION_OUTBOUND_ACTION};`（re-export 链注释明示 "A flip of `MODERATION_OUTBOUND_ACTION` is a one-line leaf edit and the A2/DB cross-pins follow automatically"）；`governance_lane_for` 映射 `LOCAL_ACTION_MODERATED → GovernanceLane { class: GOVERNANCE_CLASS_ADMIN, priority: GOVERNANCE_PRIORITY_MODERATION(=100), outbound_action: MODERATION_OUTBOUND_ACTION, status: 0 }`；6 个单元测试（含 `outbound_action_is_single_contract_token`：映射携带的是 re-export 的 token，非第二处字面量） |
| E5 | `scripts/test-integration.sh` `run_migrated_integration` empty-filter guard + 0239-gated AUDIT_CONNECTOR block | ✅ **Verified（工作树）**。empty-filter guard（:196-202）：`grep -Eq 'test result: ok\. [1-9][0-9]* passed'`，零匹配 → exit 1（"no test matched (empty-filter guard)"）——**命名条目 vacuous green 不可能**。0239-gated 块（:302-321）：`audit_governance::` + `moderation_finalize_outbox_parity` 两命名条目，各占 throwaway DB，`-f migrations/0239_audit_governance_outbox.sql` 门控，缺文件显式 SKIP。A3/T-11/priority drill 段（:322-484）同门控 |
| E6 | `docs/campaigns/implementation-gate.md:64,78`（T-11、G6） | ✅ **Verified（:63、:78 命中；:64 是行 2 起点）**。:63 aero-im 行 1 验收 = "30 个忽略测试 CI 全绿（37/37）；P2 parity"；:78 "G6（B5）| B5-1..4 | 37/37、T-11、moderation 优先级" |
| E7 | `scripts/b5-pin.sh` 37/37 具名清单 + pin guard | ✅ **Verified（untracked 新文件）**。`B5_CONTRACT_TEST_LIST` = 37 槽（**15 executed 具名 + 22 [PROPOSED] 占位**）。15 个 executed 槽及其命名过滤器：7 个 cargo-test 过滤器（`rolling_upgrade_fences_are_atomic_before_0176_reasserts_them`、`migration_0192_repairs_attempted_cross_room_scheduled_replies`、`migration_0228_backfills_workspace_bot_membership`、`migration_0233_backfills_and_constrains_human_identity_issuer`、`migration_0237_backfills_before_installing_destination_guard`、`message_quota_and_snaplink_outboxes_are_transactional`、`scim_inactive_first_nil_workspace_member_rolls_back_owner_bootstrap`——前 5 走 `run_migration_regression`，后 2 走 `run_migrated_integration`）+ `audit_governance::`、`moderation_finalize_outbox_parity`（`run_migrated_integration`，对应 `crates/aero-storage/src/audit_governance.rs:248` 同名 db_test）+ 5 个 drill/套件槽（`a3-relay-drill`、`t11-fail-closed`、`moderation-priority-drill`、`notification-fanout`、`relay-mock-probe`）+ `audit-provision-check`。`assert_b5_contract_pin`：槽数==37、无重复、无 malformed、executed≥1（全 [PROPOSED] → FAIL = vacuous 检查）、fresh 模式每 executed 槽有 `B5-CHECK <name>: PASS|SKIP` 判词行 |
| E8 | `scripts/test-b5-pin-guard.sh` guard 自测 | ✅ **Verified（untracked 新文件）**。纯 bash 无 DB；正例 + 全部反例（count≠37 / 重复 / malformed / vacuous / 判词缺失 / SKIP_DB_CREATE 降级）。由 test-integration.sh :148-149 在昂贵 DB 工作前先跑（"a pin-guard regression fails the harness before the expensive DB work"） |
| E9 | moderation-priority drill（`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`） | ✅ **Verified（untracked 新文件）**。种子 = 500 积压行（priority=10 = `GOVERNANCE_PRIORITY_BACKLOG`、`available_at` 更早）+ 1 moderation 行（class='admin'、priority=100 = `GOVERNANCE_PRIORITY_MODERATION`、outbound action = 叶子导入的 `MODERATION_OUTBOUND_ACTION`、`available_at` 更晚——排序证据只能来自 priority，绝非 FIFO）。断言：轮 1 `COUNT(status=2)==100`（batch_size，< 501 行 ⇒ 首批组合必由 priority 决定）+ moderation 行 ∈ status-2 集（**D3：成员资格 = DESC 排序契约；`delivered_at == MIN` 是执行器产物，不断言**）；全排干 `COUNT(status=2)==501` + `event_id` set-parity。能力门：0239 表缺 → exit 2 SKIP；`priority`/`class` 列缺 → exit 2 SKIP；**0239 落地但 DESC 排序未落地 → FAIL 红**（诚实的 G6 信号）。TRUNCATE-at-start 自隔离 |
| E10 | `aero-cli audit-provision-check --priority` | ✅ **Verified**。`crates/aero-cli/src/main.rs` :489-501：`"audit-provision-check"` 命令，`Some("--priority") => aero_eng::audit_provision::run_priority(&url).await`（:497），未知参数 → usage error；`aero_eng::audit_provision::run_priority`（:640）+ `priority_landed` 三态（:103、:191-192 "priority: landed/absent"）。harness 段（test-integration.sh :441-484）：RC 0 + `priority: landed` 判词 → PASS；RC 2 → SKIP (priority/class not landed)；其余 → FAIL |
| E11 | 0240 index（DESC claim） | ✅ **Verified**。`migrations/0240_audit_governance_due_prio_idx.sql`：`CREATE INDEX IF NOT EXISTS audit_governance_due_prio_idx ON audit_governance_outbox (priority DESC, available_at, created_at, event_id) WHERE status IN (0, 1)`——与 `crates/aero-audit-connector/src/pg.rs:117` `claim_due` 的 `ORDER BY candidate.priority DESC, candidate.available_at, …` 精确匹配（LIMIT pushdown，无每 tick 全量 Sort）；滚动部署 posture：0239 的 FIFO 索引保留共存 |
| E12 | T-11 drill + state_machine.rs | ✅ **Verified**。`crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs`（untracked）；harness t11-fail-closed 段（:343-439）：relay 缺席（token endpoint = 确定性关闭的 loopback 端口）⇒ 行保持 pending、`SUM(attempts)` 增长（证明 relay 真跑过，非 vacuous）、零终态、`last_error` 记录 transport failure；配给 seam leg：`audit-provision-check` 已落地则同一 DB 上必须拒发 grant（fail-closed）。`crates/aero-audit-connector/tests/state_machine.rs`：8 个集成测试（`forbidden_dead_on_first_attempt` :242、`priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane` :347 等） |
| E13 | 跨 pin（governance.rs 测试、audit_governance.rs db_tests、A2 field 断言） | ✅ **Verified**。`crates/aero-storage/src/audit_governance.rs` :320 与 :884 db_tests 断言 `gov.4["action"] == MODERATION_OUTBOUND_ACTION`（**导入叶子常量**，注释 "payload action = leaf MODERATION_OUTBOUND_ACTION (A2 half 5)"）；`:248 moderation_finalize_outbox_parity` = 37 槽命名 parity 测试。A2 field 断言锚 = 0239:123 触发器注释 + db_test :320/:884。**叶子 const + 两处 SQL 字面量（0239:123、0241:110）联动翻转 ⇒ 全部跨 pin 自动跟随；单侧翻转 ⇒ db_test 红** |

### 1.1 核对中新发现的事实（决定本 spec 的验收形态）

| # | 事实 | 来源 | 影响 |
|---|---|---|---|
| F1 | **`scripts/truth-check.sh` 的 AC4「TOKEN 单源」守卫未落地**。direction 证据引用 "truth-check AC4 literal guard" 视为现状，但实读 `scripts/truth-check.sh` 无任何 TOKEN/token_leaks/audit 类别；`rg -l 'admin.content.flag' scripts/*.sh` = 0。叶子 vocabulary-leaf 设计 doc 的 AC4（:217 "rg 扫描 = 0"、F4 缓解、§6 :221-228 给出守卫正确形态）是**设计承诺、未实现** | `scripts/truth-check.sh` 实读；`docs/design/2026-08-08-aero-common-b5-contract-vocabulary-leaf-types.design.md` :190/:217/:221-228 | 验收 (a) 增加 R4：AC4 守卫落地（§6 形态）——否则「flip 是协调 Rust+SQL 编辑」无提交门回归网，SQL 侧重引入字面量无人拦截。现状扫描已干净（`rg 'admin.content.flag' crates/ --glob '*.rs'` = 4 处，**全部在叶子 audit.rs**：doc 注释 + const + 两处测试注释/断言） |
| F2 | **全部交付物处于未提交工作树**（87 项变更；`audit.rs`、`governance.rs`、`aero-audit-connector/`、`aero-storage/src/audit_governance.rs`、`aero-eng/src/audit_provision.rs`、`migrations/0239-0241`、`b5-pin.sh`、`test-b5-pin-guard.sh` 均 untracked；`test-integration.sh`、`aero-cli/src/main.rs` 等已修改）。词汇表设计 doc §7 "基线固化"（:201）明示该状态：基线可复现 = untracked 提交 + 在途 PR 一起 | `git status --short`（87 项）；设计 doc :201 | 验收前置 P0：显式路径提交全部 B5 文件（勿 `git add -A`——工作树另有非 B5 untracked：`.pi-batch.lock`、`crates/aero-live-srt/src/isolation_tests.rs`、`docs/campaigns/audit-batch.out`） |
| F3 | **DESC 排序已在 claim 查询落地**（pg.rs:117 `ORDER BY candidate.priority DESC` 首位），drill 是**断言方**而非实现方；B5-3 切片交付物与 drill 同基线 | `pg.rs:117` 实读；`state_machine.rs:347` 有对应集成测试 | 验收 (c) 只要求 drill + 索引对齐 + harness 接线全绿，不要求改排序语义 |
| F4 | 22 个 [PROPOSED] 槽是**仓外契约名占位**（proposal :13），本 direction 明确"不臆造仓外清单"——钉入的是**槽位计数 + 判词协议**，契约文本落地时按 b5-pin.sh 头注释一键替换 | `b5-pin.sh` 头注释；proposal :13/:15 | 验收 (b) 的 37/37 = 15 具名 executed + 22 占位，计数 guard 兜底；**不得**把占位替换为臆造名 |

## 2. Verified current state

```
(a) token 裁决    叶子 audit.rs:150 MODERATION_OUTBOUND_ACTION = "admin.content.flag"（注释记录裁决：契约
                  提案列出两个 token，锁定为一个常量）；0239:123 / 0241:110 SQL 字面量同值；
                  governance.rs re-export 链 + governance_lane_for + 6 单测；db_tests :320/:884 跨 pin
(b) 37/37 钉入     b5-pin.sh（15 具名 + 22 [PROPOSED]）+ assert_b5_contract_pin（count/dupe/malformed/
                  vacuous/判词证据）+ test-b5-pin-guard.sh 自测 + test-integration.sh B5 段
                  （empty-filter guard、0239-gated 条目、A3/T-11/priority drill、relay 覆盖、pin 调用）
(c) drill          500 积压 + 1 moderation（priority DESC 成员资格断言 + 501 排干 + set-parity）；
                  0240 partial index 与 claim_due ORDER BY 精确匹配；aero-cli --priority 三态判词
(d) 回归面         state_machine.rs 8 测试（含 priority_first_claim_preempts_fifo_…）
未落地：truth-check.sh AC4 TOKEN 单源守卫（F1）；全部文件未提交（F2）
```

**现场核对命令**（复跑基线，全部 ✅ 已跑）：`rg 'admin.content.flag' crates/ --glob '*.rs'`（=4，全在叶子）；`bash -n scripts/b5-pin.sh scripts/test-b5-pin-guard.sh`（语法 OK）；`cargo check -p aero-common -p aero-ai -p aero-audit-connector -p aero-eng`（clean）。

## 3. Requirements

### R1（裁决 + 单源不变量）— token 歧义消解

叶子 `crates/aero-common/src/model/audit.rs` 是 Rust 侧**唯一合法字面量点**：`MODERATION_OUTBOUND_ACTION = "admin.content.flag"` 带裁决注释（列出两个候选 token + 锁定为一个常量）；0239/0241 SQL 字面量保持同值；`aero_ai::governance::*` 与 `aero_ai::*` re-export 链不变。

**验收**：
- R1.1 `rg -n 'MODERATION_OUTBOUND_ACTION' crates/aero-common/src/model/audit.rs` → const 定义（:150）+ `vocabulary_consts_are_pinned` 断言（:269）+ 裁决注释（:147-149 同时提到 `admin.content.flag` 与 `admin.moderation.action`）。
- R1.2 `rg 'admin.content.flag' crates/ --glob '*.rs'` 全部命中都在 `crates/aero-common/src/model/audit.rs`（排除叶子 = 0）。
- R1.3 `cargo test -p aero-common --lib`（叶子 pin）绿；`cargo test -p aero-ai --lib`（governance 6 测试）绿。

### R2（37/37 具名清单 + 反 vacuous）— parity 清单落地

`scripts/b5-pin.sh` 的 `B5_CONTRACT_TEST_LIST` = 恰好 37 个具名槽（15 executed + 22 [PROPOSED]），每个 executed 槽有命名过滤器/段；`assert_b5_contract_pin` 与 `run_migrated_integration` empty-filter guard 联合保证**零匹配即红**。

**验收**：
- R2.1 `bash scripts/test-b5-pin-guard.sh` 全反例绿（count≠37 / dupe / malformed / vacuous / 判词缺失 / SKIP_DB_CREATE 降级）——guard 自身无回归。
- R2.2 干跑 `SKIP_DB_CREATE=1 bash scripts/test-integration.sh`（或等价方式调 `assert_b5_contract_pin`）→ 输出 `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`；把清单改 36/38 槽 → FAIL（`37/N` 非零）。
- R2.3 `run_migrated_integration` 的命名 cargo-test 过滤器必须匹配 ≥1 现存测试（empty-filter guard：`test result: ok. [1-9][0-9]* passed` 缺失 → exit 1）。手工复验：`audit_governance::` → `crates/aero-storage/src/audit_governance.rs` 的 `#[ignore]` db_tests（≥1）；`moderation_finalize_outbox_parity` → `audit_governance.rs:248` 同名测试。
- R2.4 22 个 [PROPOSED] 槽保持仓外契约名占位（不臆造名称）；契约文本落地时按 b5-pin.sh 头注释一键替换，guard 自动开始要求判词证据。

### R3（moderation-priority drill）— 优先级行先达 sink

`aero-audit-priority-drill` + `aero-cli audit-provision-check --priority`：500 积压（priority 10）+ 1 moderation（priority 100、outbound action = 叶子 token）在 `priority DESC` claim（0240 index 对齐）下，moderation 行进入**首批 claimed+delivered 集**，全排干 501 行 set-parity。

**验收**：
- R3.1 `scripts/test-integration.sh` moderation-priority-drill 段（0239 文件门控）在 throwaway 迁移库全绿：exit 0 + `priority: landed` 判词 + `B5-CHECK moderation-priority-drill: PASS`。
- R3.2 drill 内部三断言（PASS 行可见）：`drill: moderation-in-first-batch: PASS`（轮 1 后 `COUNT(status=2)==100` 且 moderation 行 ∈ 集）、`drill: drain-501: PASS`（`COUNT(status=2)==501`）、`drill: parity-501: PASS`（event_id set-parity）。
- R3.3 能力门语义：0239 表或 `priority`/`class` 列缺 → exit 2 SKIP（判词 `SKIP (priority/class not landed)`）；**0239 落地但 DESC 排序缺失 → FAIL 红**（诚实 G6 信号，不 SKIP）。
- R3.4 手工复验（可选）：`DATABASE_URL=… cargo run -p aero-audit-connector --bin aero-audit-priority-drill` 于已迁移 throwaway 库 → 三 PASS 行 + exit 0。

### R4（AC4 TOKEN 单源守卫）— 补落地缺口（F1）

`scripts/truth-check.sh` 新增「TOKEN 单源」硬违规类别（vocabulary-leaf 设计 doc §6 :221-228 形态）：`rg -l 'admin.content.flag' crates/ --glob '*.rs'` 排除叶子文件后命中 = 0；**正确形态**（`|| true` 惯例 + 单一 exit 点，避免 `set -euo pipefail` 下 rg 零命中误红 / 命中时管道漏报的双向失效）。

**验收**：
- R4.1 `scripts/truth-check.sh` 含 TOKEN 单源类别，违规输出 `TOKEN LEAK: admin.content.flag …` 且脚本非零退出。
- R4.2 当前库态下 `scripts/truth-check.sh` 全绿（4 处命中全在叶子，排除后 0）。
- R4.3 负例：临时在任一非叶子 crate 写入字面量 → truth-check 红；删除后恢复绿。

### R5（回归面）— T-11 drill + state_machine.rs

**验收**：
- R5.1 `scripts/test-integration.sh` t11-fail-closed 段在 throwaway 迁移库全绿：`B5-CHECK t11-fail-closed: PASS`；配给 seam（`audit-provision-check` 已落地时）同一 relay-absent DB 拒发 grant（fail-closed，exit 非零）。
- R5.2 `cargo test -p aero-audit-connector --test state_machine`（8 测试）绿，含 `forbidden_dead_on_first_attempt`、`priority_first_claim_preempts_fifo_and_limit1_keeps_top_lane`。
- R5.3 干跑/全跑不破坏既有 B5 槽：A3 drill、notification-fanout、relay-mock-probe 判词齐备（relay 覆盖断言 legs ≥1）。

## 4. Acceptance mapping（direction (a)–(d) → 可测检查）

| Direction acceptance | 本 spec 覆盖 | 可测命令 |
|---|---|---|
| (a) Contract decision recorded; flip = one-leaf edit + 2 SQL literal edits, cross-pins follow | R1 + R4 | `rg` 扫描（R1.2/R4.2）；`cargo test -p aero-common --lib`、`cargo test -p aero-ai --lib`（R1.3）；throwaway 迁移库 `cargo test -p aero-storage --lib audit_governance:: -- --ignored --test-threads=1`（db_test :320/:884 跨 pin 绿） |
| (b) 37/37 named entries with named filters; FAILS on 0 matched tests | R2 | `bash scripts/test-b5-pin-guard.sh`；`SKIP_DB_CREATE=1` 干跑 pin 判词（R2.2）；empty-filter guard 负例（R2.3） |
| (c) priority drill: flag row ahead of priority-10 backlog under DESC claim (0240 index) | R3 | harness 段全跑（R3.1）；drill 三 PASS 行（R3.2）；能力门语义（R3.3） |
| (d) T-11 drill + state_machine.rs stay green | R5 | harness t11 段（R5.1）；`cargo test -p aero-audit-connector --test state_machine`（R5.2） |

## 5. Boundaries（不扩展范围）

- **不做契约翻转**：裁决已锁定 `admin.content.flag`；翻转是未来偶发事件，本 spec 只保证「翻转 = 叶子 1 行 + 0239:123/0241:110 两处 SQL 联动」的编辑协议与回归网（R1/R4）。
- **不实现 B5-3 排序本身**：`pg.rs:117` DESC claim 已落地（F3），drill 是断言方。
- **不改 37 槽构成**：15 executed + 22 [PROPOSED] 保持；仓外契约名不臆造（F4）。
- **不实现 B5-4 auth 侧配给门 / B5-2 relay 本体 / L1 聚合**：sibling 切片职责，本 spec 只接线判词与 drill 消费。
- **不动 0239/0240/0241 DDL**：SQL 字面量是 DB 侧单源（SQL 无法 import Rust），仅经 db_tests 交叉互钉。
- **零新增迁移、零新增第三方依赖、connector `src/` 零改动**（drill bins 为新增文件）。

## 6. 提交前置（P0，process gate）

按 vocabulary-leaf 设计 doc §7 基线固化：**显式路径 `git add <paths>`** 提交全部 untracked B5 文件（connector crate、`governance.rs`、`audit_provision.rs`、`audit_governance.rs`、drills、`migrations/0239-0241`、`b5-pin.sh`、`test-b5-pin-guard.sh`、`model/audit.rs`）+ 在途 PR 另一半（root Cargo、aero-ai lib/worker、aero-eng、aero-server、aero-storage lib.rs、`test-integration.sh`）。⚠️ 勿 `git add -A`（工作树另有非 B5 untracked：`.pi-batch.lock`、`crates/aero-live-srt/src/isolation_tests.rs`、`docs/campaigns/audit-batch.out`）。提交后 `git status --short` 无 B5 文件残留。
