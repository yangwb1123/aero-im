# Requirements Spec — `audit-provision-check --priority` 破坏性门禁 + B5-3 出站 action 词汇表钉入（aero-cli / aero-eng 切片）

- **Module (analysis root)**: `crates/aero-cli/src` — aero-eng 工程 CLI（`[[bin]] name="aero-eng"`）；本 direction 交付物横跨 `crates/aero-eng/src/audit_provision.rs`（`run_priority` 门禁）、`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（词汇表断言）、`scripts/test-integration.sh`（priority 腿 opt-in + 负例子检查）
- **Direction**: "Harden `audit-provision-check --priority` against destructive runs and pin the full moderation action vocabulary the contract names"（value 6 / risk_reduction 6 / effort 3 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-cli-src-864950e4.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（:10 B5-3 映射表 = 出站 `admin.content.flag`/`admin.moderation.action`）；gate anchor `docs/campaigns/implementation-gate.md`（:65 "Moderation 优先级：`admin.content.flag`/`admin.moderation.action` 先于积压"、:78 G6）
- **Sibling specs（同模块/同批次，命令面协调）**: `docs/requirements/2026-08-08-aero-cli-b5-3-moderation-priority-drill.req.md`（`--priority` 面本体 + AC1–AC6）；`docs/requirements/2026-08-08-aero-ai-b5-3-token-parity-harness.req.md`（token 歧义裁决 = **不翻转**，叶子锁 `admin.content.flag`，R1 单源不变量 + R4 AC4 守卫）；`docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（命令本体）。本 direction = 前者**加固修订**（D8 设计裁决被推翻，见 F1），与 token-parity 的「单源不变量」互补不冲突
- **Status**: Requirements（下述证据全部经源码实读核对）
- **Verification date**: 2026-08-08。行号是核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点（AGENTS.md §0）

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs` — `run_priority` (:640) 警告文本 + `AERO_PRIORITY_DRILL_BIN` 直 spawn (:675)；base `run()` 先行；**无 spawn 前 outbox 非空守卫** | ✅ **Verified（:640-718 精确）**。`run_priority` = ① base `run()` 先行（:644-651，`base.is_error()` 即返回——坏 DB 先红）→ ② 列门（`parse_db_url` :652-653 + `PsqlRunner` :654 + `probe_priority_columns` :655-661；缺列 → `Outcome::warning(2, …)` SKIP :662-667）→ ③ 警告 println :672-676（"WARNING — the drill TRUNCATEs audit_governance_outbox; run against a throwaway DB only"）→ spawn（`AERO_PRIORITY_DRILL_BIN` env override :677、`cargo run -p aero-audit-connector --bin aero-audit-priority-drill` fallback :678-686、`DATABASE_URL` 注入 :688-691、120s timeout :697、exit 码透传 0/1/2 :698-711）。**②③之间无任何 outbox 行数探测**——psql 探测设施（`PsqlRunner::query` :441、`probe_priority_columns` :509-515、`parse_priority_probe` :226、`P_SQL` :62）现成可复用 |
| E2 | `crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs:105-113` — 开跑 TRUNCATE `audit_governance_outbox`（'TRUNCATE-at-start guard'） | ✅ **Verified（:105-113 精确）**。注释 :105-108 "Self-isolating start (db-reviewer finding 3)"；`sqlx::query("TRUNCATE audit_governance_outbox")` :109-111；println :113 "reset audit_governance_outbox (TRUNCATE-at-start guard)"。前置能力门：0239 表缺 → exit 2 SKIP（:92-103）；`priority`/`class` 列缺 → exit 2 SKIP（:115-133） |
| E3 | `crates/aero-cli/src/main.rs` — `AuditProvisionCheck_`：未知 flag 响亮 usage error（silent-wrong-result 姿态），但 `--priority` 无破坏性门 | ✅ **Verified（:488-506）**。`c!(AuditProvisionCheck_, …)` :488-506：无 flag → `run()`（:496）；`Some("--priority")` → `run_priority(&url)`（:497）；其他 → `Outcome::error("usage: audit-provision-check [--priority]")`（:501，exit 1）。`Outcome::error` → exit 1（`crates/aero-eng/src/outcome.rs`），main.rs :42-45 把 message 打到 stderr 后 `exit(r.exit_code())`。**无破坏性门** |
| E4 | `scripts/test-integration.sh:443-484` — moderation-priority drill 腿在全新 throwaway DB 上（harness 侧安全，CLI 侧守卫缺失） | ✅ **Verified（腿 = :448-484）**。注释 :448-450（"500 backlog rows + 1 moderation row"）；`:37 PRIORITY_DRILL_DB="aero_priority_drill_$$"` + `:61 assert_disposable_db_name`；:455-461 建库 + migrate；:462-467 跑 `cargo run --quiet -p aero-cli -- audit-provision-check --priority`（env 仅 `DATABASE_URL`/`AERO__DATABASE__URL`——**无任何 opt-in env**）；:471-478 RC 0 → grep `priority: landed` + `b5_check "moderation-priority-drill" "PASS"`；:479-481 RC 2 → SKIP；:482-484 其他 → 红。B1/B2 腿 :256-305、D 腿 :396-421、t11-fail-closed 段 :343-447 均以**无 `--priority`** 调用 → 不受门禁影响 |
| E5 | `crates/aero-ai/src/governance.rs:31-60` + 0239 header — `MODERATION_OUTBOUND_ACTION` 是唯一出站 token（'admin.content.flag'）；'admin.moderation.action' 在 crates/ migrations/ scripts/ 无处出现 | ⚠️→✅ **基本属实 + 一处勘误**。叶子 `crates/aero-common/src/model/audit.rs`：`MODERATION_OUTBOUND_ACTION = "admin.content.flag"`（:150），裁决注释 :146-150（"The contract proposal listed both `admin.content.flag` and `admin.moderation.action`; locked here as ONE constant"），pin 测试 :269。`governance.rs`：re-export 链 :39-44（doc :31-38），`GOVERNANCE_PRIORITY_MODERATION=100` :31、`GOVERNANCE_PRIORITY_BACKLOG=10` :33。0239 header 钉 :16-17，SQL 字面量 0239:98,123 + 0241:110。**勘误**：'admin.moderation.action' **出现在恰好一处**——叶子 doc 注释 audit.rs:147。direction 的「appears nowhere」不精确；实质成立：**作为 token/常量/DDL 字面量/断言，第二契约 action 零表示**（Sibling token-parity spec E2 同口径） |
| E6 | `crates/aero-storage/src/audit_governance.rs:320-321,884` — payload action 断言只钉 `MODERATION_OUTBOUND_ACTION` | ✅ **Verified（:320-321、:884 精确）**。两处 db_test 均 `assert_eq!(gov[…]["action"], MODERATION_OUTBOUND_ACTION, …)`（导入叶子常量；注释 "payload action = leaf MODERATION_OUTBOUND_ACTION (A2 half 5)"）。全文件无第二契约 action 的任何表示 |
| E7 | （补充）**drill 只断言单 action，且投递后从不读回 payload action** | ✅ **Verified**。drill 种子 1 moderation 行（:174-193，payload `"action": MODERATION_ACTION`，`:69-71 MODERATION_ACTION = MODERATION_OUTBOUND_ACTION` 单常量）；断言 = 轮 1 claimed==100（:207-212）、moderation 行 ∈ 首批发货集（:216-244 → `drill: moderation-in-first-batch: PASS` :244，D3 成员资格语义）、`drain-501`（:246-279）、`parity-501`（:281-299）。**投递后只读 `delivered_at`（:216-227），从不重读 payload `action`**——词汇表断言不存在 |
| E8 | （补充）**契约 item (3) 的仓内文本锚** | ✅ **Verified**。`docs/proposals/audit-contract-batch-aero-im.md:10`："本地 `message.moderated` → 出站 `admin.content.flag`/`admin.moderation.action` 映射表；注入积压 drill（500 积压 + 1 moderation → 先达 sink）+ 反饥饿上限"；`docs/campaigns/implementation-gate.md:65`："Moderation 优先级：`admin.content.flag`/`admin.moderation.action` 先于积压"。两 token 并列，无单一裁决——sibling 已裁决不翻转（叶子锁第一个） |
| E9 | （补充）**设计裁决 D8 = "不做硬门禁"（本 direction 要推翻的决策）** | ✅ **Verified**。`docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md` :32："D8 — 破坏性警告 … 不做硬门禁（harness 与历史契约都依赖无 gate 直跑）"。本 direction = D8 的显式修订（D8′）：警告升级为条件硬门禁，harness 以 opt-in 保持直跑 |
| E10 | （补充）**回归面**：`crates/aero-eng/tests/audit_provision.rs` | ✅ **Verified**。**勘误（2026-08-08 复核）**：全文件实际 **25 个 `#[test]`**（8×`verdict_*` :29-173、3×`report_*` :195-266、2×`parse_priority_probe_*` :269-283、1×`report_priority_and_class_verdict_lines` :288、1×`parse_psql_bool_line`、1×`parse_relay_line`、1×`parse_probe_line`、1×`parse_buckets`、1×`parse_v1_line`、1×`parse_dead_rows`、4×`parse_db_url_*`、1×`run_without_url`）——本行「14」只枚举 verdict/report/priority-probe 子集；**零改动回归面 = 25**。全部无 DB、无 spawn；`run()`/`run_priority()` 无集成测试。AC4 的"字节级不变"映射 = `run()`/`verdict`/`format_report`/`parse_buckets`/`parse_priority_probe` 零改动 + 既有测试零改动 |
| E11 | （补充）**37-pin 与槽位** | ✅ **Verified**。`scripts/b5-pin.sh`：:23 "37 slots: 15 executed + 22 [PROPOSED]"；:41 槽 `moderation-priority-drill`；:80-95 `assert_b5_contract_pin`（count==37 / 无重复 / 无 malformed / executed≥1 / 判词证据）。门禁与负例检查均**不新增槽** → 37 不变 |
| E12 | （补充）**drill 常量与退出码契约** | ✅ **Verified**。drill :55-73：`MAX_ROUNDS=10`、`BACKLOG_PRIORITY=10`、`MODERATION_PRIORITY=100`、`MODERATION_ACTION=MODERATION_OUTBOUND_ACTION`、`BACKLOG_ROWS=500`、`MODERATION_ROWS=1`、`TOTAL_ROWS=501`、`BATCH_SIZE=100`。退出码：0 PASS / 1 FAIL / 2 SKIP（能力门），CLI 透传（E1） |

### 1.1 核对中新发现的事实（决定本 spec 的验收形态）

| # | 事实 | 来源 | 影响 |
|---|---|---|---|
| F1 | **设计 D8（"不做硬门禁"）与本 direction 正面冲突**——本 spec 的 R1 = D8′ 修订，须在 `docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md` 记修订注记 | design doc :32 | R1 的合法化依据：direction 明确提案门禁，D8 的"历史契约依赖无 gate 直跑"由 harness opt-in（R2）承接 |
| F2 | **base 判定会把 0239 pending/claimed 计入 undelivered**：`verdict()` :133-145 `undelivered = v1.pending + v1.claimed + g0239.pending + g0239.claimed`，relay 关 + undelivered>0 → fail-closed。**status 0/1 行会让 base 先红、走不到守卫**——guard 的边际保护面 = base 判 Consistent/Healthy 时仍存在的行（典型 = 已投递 status=2 的审计历史，如共享 dev DB 上跑过一次 drill 后残留 501 行 delivered） | `audit_provision.rs:133-145` 实读 | ① 守卫必须放 base 之后、spawn 之前（base 先行语义保留）；② 计数用 `COUNT(*)`（任何 status——acceptance 原文 "non-empty audit_governance_outbox"）；③ **harness 负例必须种 status=2 行**才能打中守卫本身（种 status 0/1 只会测到 base fail-closed，非本 direction 新增行为） |
| F3 | **harness 腿当前不设 opt-in**；且若在腿内预置行，drill 轮 1 的 `COUNT(status=2)==100` 断言会被预置 delivered 行破坏 → 负例检查后必须 DELETE 预置行再跑 drill | test-integration.sh :462-467 实读；drill :207-212 实读 | R2 形态：负例检查（insert → 无 opt-in 跑 → 断言 REFUSED + 行存活 → delete）内嵌进现有 priority 腿，additive，不新增 b5 槽 |
| F4 | **全部 B5 交付物处于未提交工作树**（`git status --short`：`audit_provision.rs`、`main.rs`、`test-integration.sh` 等 M；`aero-audit-connector/`、`governance.rs`、`audit.rs` 等 ??；另有非 B5 untracked：`.pi-batch.lock`、`crates/aero-live-srt/src/isolation_tests.rs` 等） | `git status --short` 实跑 | 提交门 P0：显式路径提交 B5 文件，**勿 `git add -A`**（sibling token-parity spec F2 同款） |
| F5 | `scripts/truth-check.sh` 尚无 TOKEN 类别（sibling token-parity F1 已记）——**不在本 direction 范围**（sibling R4 交付），本 spec 只在 §5 划界 | `grep TOKEN scripts/truth-check.sh` = 0 命中 | 词汇表钉入靠 drill 内断言 + 单测（R3），不依赖 truth-check 类别 |

## 2. Verified current state

```
--priority 破坏面（现状三条独立事实）：
a) CLI（E1/E3）   run_priority = base → 列门 → WARNING(:672-676) → 直 spawn drill；
                  唯一防护 = 一行警告文本；AERO_PRIORITY_DRILL_BIN 是既有 env override 先例
b) drill（E2/E7） 开跑即 TRUNCATE audit_governance_outbox（:109-111，无条件）；
                  种子 500×prio-10 + 1×prio-100(class admin, action=叶子 token)；
                  断言 moderation-in-first-batch / drain-501 / parity-501——投递后不读 payload action
c) harness（E4）  腿 :448-484 在 throwaway 库跑，无 opt-in env——harness 侧安全，
                  CLI 侧无守卫 = 共享 dev DB 上 `aero-eng audit-provision-check --priority`
                  静默销毁 status 0/1/2 行（F2：status 0/1 时 base 先 fail-closed 已拦住，
                  守卫的边际保护 = status 2 历史行 + 未来判定放宽时的兜底）

词汇表现状（E5/E6/E7）：
  契约（E8）:10/:65 并列两个出站 action；叶子锁 'admin.content.flag'（sibling 裁决不翻转）；
  树内 'admin.moderation.action' 仅叶子 doc 注释 1 处；drill/存储断言全部只钉单 token；
  第二契约 action 零表示、零断言——AC3 的缺口本体

门禁设施现成（E1）：PsqlRunner::query + probe_priority_columns 模式 + parse_priority_probe 纯函数模式
```

**Gaps this direction closes**（all verified）：① `--priority` 无破坏性门（D8 设计裁决，F1）——R1 条件门禁 + R2 harness opt-in/负例；② 第二契约 action 无表示无断言（E5/E6/E7）——R3 词汇表硬编码 + 种子期成员检查 + 投递后读回断言。

## 3. Requirements

### R1 — `run_priority` 破坏性门禁（AC1 前半）

`crates/aero-eng/src/audit_provision.rs`，插入点 = `run_priority` 步骤 2（列门）与步骤 3（警告 + spawn）之间：

- **计数探测**：`runner.query("SELECT COUNT(*)::bigint FROM audit_governance_outbox").await?`（复用 :654 已建的 `PsqlRunner`；psql `-At` 单值契约；探测失败 → `Outcome::error` fail-closed，**绝不带病 spawn**）。
- **纯函数**（镜像 `parse_priority_probe` :226 模式，可单测）：
  - `pub fn parse_outbox_count(out: &str) -> Result<i64, String>` —— trim + parse，垃圾输入 Err；
  - `pub fn priority_drill_blocked(count: i64, allow_truncate: bool) -> bool` —— `count > 0 && !allow_truncate`。
- **opt-in env**：`std::env::var("AERO_PRIORITY_DRILL_ALLOW_TRUNCATE").as_deref() == Ok("1")`（plain 单下划线 env，与 `AERO_PRIORITY_DRILL_BIN` :677 同款；值语义 `== "1"` 与 `AERO_SAML_EXPERIMENTAL_VERIFY=1` 先例一致）。**不新增 CLI flag**——`args` 面保持字节级不变（AC4），且与 acceptance 的 env 拼写一致。
- **拒绝路径**：`count > 0 && !allow` → stdout println `audit-provision-check: priority-drill: REFUSED — audit_governance_outbox has {count} row(s); the drill TRUNCATEs the table. Re-run with AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 (throwaway DB only)` + `Outcome::error(同文案)` → **exit 1**（非零 + 显式消息，经 main.rs :42-45 stderr 输出）。
- **放行路径**：`count == 0 || allow` → 原警告（:672-676）+ spawn 流程**逐字节不变**。
- **顺序不变量**：base `run()` 先行（:644-651）与列门 SKIP exit-2（:662-667）**原样保留**——守卫只在 base 判定 Consistent/Healthy 后生效（F2：status 0/1 行本就被 base fail-closed 拦截；守卫的边际保护 = status 2 历史行）。

**可测验收**：
1. `cargo test -p aero-eng --lib` + `--test audit_provision`：新增 `parse_outbox_count` / `priority_drill_blocked` 单测绿；**既有 25 测试零改动全绿**（E10 勘误；AC4 后半）。
2. 手工/脚本复验（throwaway 库，见 R2 负例子检查）：migrate 后 INSERT 1 行 status=2 → 无 opt-in 跑 `audit-provision-check --priority` → exit 1 + 日志含 `REFUSED` + `SELECT COUNT(*)` 仍为 1（TRUNCATE 未发生）；带 opt-in 再跑 → drill 正常（exit 0 / 自身 SKIP 契约不变）。
3. 空 outbox（全新 throwaway 库）→ 无 opt-in 也放行（`COUNT(*)==0` 不触发守卫）——harness 腿无需 opt-in 也能绿，但按 AC1 仍设（R2）。

### R2 — harness 腿 opt-in + 门禁负例子检查（AC1 后半 + AC2）

`scripts/test-integration.sh` moderation-priority drill 腿（:448-484），**仅此腿**：

- **opt-in**：:465-467 env 块加 `AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1 \`——显式声明「本腿授权 TRUNCATE」（throwaway 库，语义自证；fresh 库 COUNT=0 本不触发守卫，opt-in 是意图声明 + 对残留行的兜底）。
- **负例子检查**（additive，插在 migrate 后、正式 drill 前）：
  1. INSERT 1 行 status=2 进 `audit_governance_outbox`（payload 形状照 drill 种子 `{"event_id": <uuid>, "source_system": "aero-im.source"}`；**必须 status=2**——F2：status 0/1 会让 base 先 fail-closed，测不到守卫）；
  2. 无 opt-in 跑 `DATABASE_URL=… cargo run --quiet -p aero-cli -- audit-provision-check --priority` → 断言 `RC != 0` 且日志含 `REFUSED` 且 `SELECT COUNT(*)` 仍为 1；
  3. DELETE 预置行（**必须**——否则 drill 轮 1 的 `COUNT(status=2)==100` 断言被预置 delivered 行破坏，drill :207-212）；
  4. 正式 drill 带 opt-in 跑（:465-467 已加）→ 原流程不变：RC 0 + grep `priority: landed` + `b5_check "moderation-priority-drill" "PASS"`；RC 2 → SKIP；其余 → 红。
- **零改动面**：B1/B2 腿（:256-305）、D 腿（:396-421）、t11-fail-closed 段（:343-447）——均无 `--priority` 调用，一字不动；`scripts/b5-pin.sh` 37 槽一字不动（负例检查不产生新 `b5_check` 槽，`moderation-priority-drill` 槽判词来源不变）。

**可测验收**：
1. `bash scripts/test-integration.sh`（或等价 B5 段）→ `moderation-priority-drill` 腿 PASS（exit 0 + `priority: landed`），且日志同时含负例 REFUSED 断言通过痕迹（腿内 `echo "✓ priority-drill guard: REFUSED on non-empty outbox"` 之类行）。
2. `bash scripts/test-b5-pin-guard.sh` 绿 + `B5 contract pin: 37/37` 判词不变（AC2）。
3. 故意去掉 opt-in env 重跑 → 腿仍绿（fresh 库 COUNT=0）；故意把负例 INSERT 改成 status=0 → 断言应观察到 base fail-closed 而非 REFUSED（文档化 F2 语义，非断言）。

### R3 — drill 出站 action 词汇表钉入（AC3）

`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（**行数结构零改动**：500+1=501、`BATCH_SIZE=100`、`MODERATION_ROWS=1`、成员资格断言语义不变）：

- **词汇表常量**（契约字面量**硬编码**，不从叶子派生——派生会使翻转自动跟随、pin 失效）：
  ```rust
  /// Contract item 3 outbound vocabulary (proposal :10, implementation-gate.md:65;
  /// leaf audit.rs:146-150 locks ONE constant — either spelling is contract-legal).
  const MODERATION_OUTBOUND_ACTIONS: [&str; 2] = ["admin.content.flag", "admin.moderation.action"];
  ```
- **种子期检查**（在 `MODERATION_ACTION` 定义处或 seed 前）：`MODERATION_OUTBOUND_ACTIONS.contains(&MODERATION_ACTION)` 否则 `anyhow::bail!("leaf MODERATION_OUTBOUND_ACTION {MODERATION_ACTION} is outside the contract vocabulary {MODERATION_OUTBOUND_ACTIONS:?}")`——叶子若被翻出契约外值，drill **红 FAIL**（exit 1，经 CLI 透传）。
- **投递后读回断言**（镜像 :216-227 的 `delivered_at` 读回形态，置于 `moderation-in-first-batch` 检查后）：`SELECT payload->>'action' FROM audit_governance_outbox WHERE event_id = $1`（moderation 行，投递后）→ 断言 `MODERATION_OUTBOUND_ACTIONS.contains(&action)` 否则 bail（"delivered moderation row action {action} not in contract vocabulary"）→ `println!("drill: moderation-action-vocabulary: PASS")`（PASS 行经 CLI inherit 透传，harness 可见）。
- **单测**（bin 内 `#[cfg(test)]`）：词汇表恰为契约对（逐字量断言，镜像叶子 `vocabulary_consts_are_pinned` :269 形态）+ 包含 `MODERATION_ACTION`。
- **不变面**：`moderation-in-first-batch` / `drain-501` / `parity-501` 三 PASS 行、`TRUNCATE-at-start`、能力门 exit-2、种子行数——全部原样（AC3 "membership in the first batch (batch_size=100, B5-3 D3) is unchanged"）。

**可测验收**：
1. `cargo test -p aero-audit-connector --bin aero-audit-priority-drill`（bin 单测）绿。
2. throwaway 迁移库跑 drill（harness 腿或手工）：日志含 `drill: moderation-action-vocabulary: PASS` + 既有三 PASS 行 + exit 0。
3. 负例（临时）：把 `MODERATION_ACTION` 改为 `"mod.flag"` → drill 种子期 bail / 投递读回 bail → exit 1（红）；还原后绿。
4. 契约翻转兼容：若未来契约裁决翻叶子到 `"admin.moderation.action"`，本断言仍 PASS（两拼写皆合法）——与 sibling token-parity R1 的单源翻转协议一致。

### R4 — 回归面：base 行为字节级不变（AC4）

- **零改动**：`run()`（:527-638）、`verdict`（:125）、`format_report`（:150）、`parse_buckets`（:275）、`parse_priority_probe`（:226）、`probe_priority_columns`（:509）、base 判词行（`relay:`/`v1-outbox:`/`outbox-0239:`/`oldest-pending-age:`/`dead:`/`priority:`/`class:`/`verdict:` 全系列）——本 direction 只增 R1 的守卫分支 + 两个纯函数，不触碰上述任何符号的输出。
- `crates/aero-cli/src/main.rs` `AuditProvisionCheck_` 参数面零改动（env opt-in，不加 flag）；help 描述文本可加一句 "refuses on a non-empty outbox unless AERO_PRIORITY_DRILL_ALLOW_TRUNCATE=1"（additive，可选）。

**可测验收**：
1. `cargo test -p aero-eng --lib` + `--test audit_provision`（25 既有 + 新增纯函数测试，E10 勘误）全绿。
2. harness B1/B2/D 腿判词 grep（`verdict: consistent` / `verdict: fail-closed` / `dead=1` / `delivered=0` / `audit-provision-check: dead:` / `priority: landed`）逐字节原样通过（B1/B2/D 腿零改动即证明——E4）。
3. 变更前后对同一 migrated throwaway 库跑 `audit-provision-check`（无 flag）输出 `diff` 为空。

## 4. Acceptance mapping（direction (a)–(d) → 可测检查）

| Acceptance | Requirements | Executable check |
|---|---|---|
| (a) 非空 outbox 上 `--priority` 退出非零 + 显式消息，除非 opt-in；harness 腿设 opt-in 保持绿（moderation-priority-drill PASS, exit 0） | R1 + R2 | R1.2 负例（REFUSED + exit 1 + 行存活）与放行（opt-in → drill）；R2 腿 PASS 判词 + 负例子检查 echo；R1.3 空 outbox 放行 |
| (b) B1/B2/D 与 t11-fail-closed 腿不变；37-pin 计数不变（guard 是 additive） | R2 | R2「零改动面」；`bash scripts/test-b5-pin-guard.sh` + `B5 contract pin: 37/37` 判词 |
| (c) drill 断言 moderation 行出站 action ∈ {admin.content.flag, admin.moderation.action}；class='admin' + priority=100 首批成员资格（batch_size=100, B5-3 D3）不变 | R3 | R3.2 `drill: moderation-action-vocabulary: PASS` + 既有三 PASS；R3.3 负例（leaf 翻出词汇表 → 红）；R3.4 双拼写兼容 |
| (d) 无 flag 行为与判词字节级不变；aero-eng/tests/audit_provision.rs 无回归 | R4 | R4.1 cargo test 全绿（25 既有零改动，E10 勘误）；R4.2 B1/B2/D grep 判词原样；R4.3 输出 diff 为空 |

## 5. Scope

**In scope**：`crates/aero-eng/src/audit_provision.rs`（R1 守卫 + 纯函数 + 单测）；`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`（R3 词汇表常量 + 种子期检查 + 投递后读回断言 + bin 单测）；`scripts/test-integration.sh` priority 腿（R2 opt-in + 负例子检查）；`docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md` D8 修订注记（D8′，记录"警告→条件门禁"的裁决变更与理由）；`crates/aero-cli/src/main.rs` help 描述一句话（可选，additive）。

**Out of scope（并行切片 / 其他模块——勿在本 direction 建造）**：
- **token 翻转/裁决**：叶子保持 `"admin.content.flag"`（sibling `2026-08-08-aero-ai-b5-3-token-parity-harness.req.md` R1 已裁决；本 direction 只钉「两拼写皆契约合法」的断言面，不翻转、不新增第二常量到叶子）。
- **0239/0240/0241 DDL、claim 排序语义、反饥饿 cap**：aero-storage/connector 既有交付，零触碰（E12；sibling D-CAP 另批）。
- **B1/B2/D/t11 腿与 37 槽清单**：零改动（R2 零改动面）。
- **`scripts/truth-check.sh` TOKEN 类别**：sibling token-parity R4 交付物（F5），本 direction 不建。
- **drill 行数结构 / 批大小 / 成员资格语义**：500+1=501、batch_size=100、D3 成员资格断言全部原样（R3 不变面）——**不**加第二 seed 行（那会改 `drain-501`/`parity-501` 计数与 sibling R3.2 钉死的 PASS 行，超出 AC3 "membership … unchanged"）。
- **CLI flag 面**：不加 `--force` 等 flag（env opt-in 即 acceptance 拼写）；`args` 解析零改动。
- **真实 sink/IdP 联调**（仓外）：不动。

## 6. Process gate

- **P0 提交门**：工作树 B5 在途（F4），提交**显式路径**列出本 direction 交付物（`crates/aero-eng/src/audit_provision.rs`、`crates/aero-audit-connector/src/bin/aero-audit-priority-drill.rs`、`scripts/test-integration.sh`、`docs/design/2026-08-08-aero-cli-b5-3-moderation-priority-drill.design.md`、本 spec），**勿 `git add -A`**（非 B5 untracked：`.pi-batch.lock`、`crates/aero-live-srt/src/isolation_tests.rs` 等）。
- **P1 验证顺序**：`cargo check --workspace` → `cargo test -p aero-eng --lib --test audit_provision` → `cargo test -p aero-audit-connector --bin aero-audit-priority-drill` → `bash scripts/test-b5-pin-guard.sh` → 带 DB 的 B5 段（throwaway 库）。
- **P2 契约窗口**：AC3 的词汇表断言不依赖契约文本落仓（两拼写已在仓内 proposal :10 / gate :65 具名）；若仓外契约最终只裁决一个拼写，词汇表常量收窄为单元素是 R3.4 的兼容路径（断言仍绿）。
