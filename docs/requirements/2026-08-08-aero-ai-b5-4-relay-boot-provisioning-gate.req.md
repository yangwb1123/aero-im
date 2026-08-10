# Requirements Spec — aero-ai B5-4：provisioning 环闭合（relay boot 接入 fail-closed verdict + 37/37 契约清单钉入）

- **Module (analysis root)**: `crates/aero-ai/src`（governance.rs = 映射权威；`AiWorker::handle_moderate` = 被 gate 评估的 audit 行生产者；boot 接线落 sibling aero-server，本 spec 钉契约与 oracles）
- **Direction**: "Close the B5-4 scope-provisioning loop in-repo: wire audit-provision-check into relay boot as a fail-closed gate and pin the 37/37 contract list in test-integration.sh"（value 6 / risk_reduction 8 / effort 5 / confidence 7）
- **Source analysis**: `docs/auto/analyses/crates-aero-ai-src-1bbf99ce.json`（direction #3）
- **Campaign**: `aero-im-b5-outbox-relay`；contract anchor `docs/proposals/audit-contract-batch-aero-im.md`（:11 B5-4 seam、:13 [PROPOSED] 含 37/37 清单、:15 "37/37 需先把契约测试清单钉入 test-integration.sh"）；gate anchor `docs/campaigns/implementation-gate.md:78`（G6 = "37/37、T-11、moderation 优先级"）
- **Sibling specs（同契约不同切片，勿撞）**: `2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（seam 本体，已落地）、`2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md`（auth 侧 gate seam + 心跳，本 direction 不建）
- **Status**: Requirements（下述证据全部经源码 grep + 实跑核对；行号为核对时锚点，可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs`（Q0 runtime.enabled+bindings 谓词、Q1 v1 桶、Q3 0239 桶、`Verdict::FailClosed` "no audit:event:write grant issued"） | ✅ **Verified**（562 行）。Q0_SQL :37 = `SELECT runtime.enabled, (SELECT count(*) FROM snaplink_commercial_bindings WHERE enabled) FROM snaplink_commercial_runtime runtime WHERE runtime.singleton`（doc :6-8：relay-health 谓词读 DB 开关 + enabled bindings，**刻意不用进程内 `ready()`**）；Q1_SQL :41（`destination='audit'`，undelivered = `delivered_at IS NULL`，镜像仓储 `pending_count()`）；Q2/Q3/Q4/Q5 :45-55（四桶 0/1/2/3、oldest-pending 秒数、dead 明细 ≤5）；`G0239_CANDIDATES: [&str;2] = ["audit_governance_outbox","audit_outbox"]` :26（漂移单点）；`verdict()` :109-122 三态矩阵 **dead 优先**（"dead is never counted delivered"）；`Verdict::FailClosed("relay disabled (bindings=…) with K undelivered audit row(s); no audit:event:write grant issued")` :117-120；`format_report` :125-166（全部 `audit-provision-check:` 前缀，leg grep 面）；`run(db_url)` :470-536（psql 直 spawn、QUERY_TIMEOUT 30s、local/container 双模式、**非判定路径永 exit 1**）。 |
| E2 | `crates/aero-eng/tests/audit_provision.rs`（`verdict_relay_off_with_undelivered_is_fail_closed` et al.） | ✅ **Verified + 实跑**（384 行）。`verdict_relay_off_with_undelivered_is_fail_closed` :18-33（reason 含 "no audit:event:write grant issued" + "relay disabled"）；另有 `verdict_relay_off_claimed_rows_also_fail_closed` / `verdict_relay_off_empty_is_consistent` / `verdict_relay_on_bindings_zero_is_fail_closed_when_undelivered` / `verdict_relay_on_with_backlog_is_healthy` / `verdict_dead_is_fail_closed_even_with_relay_on` :106-132 / `verdict_dead_priority_over_relay_disabled` :136-153 / `verdict_0239_undelivered_counts_with_relay_off`。**实跑 `cargo test -p aero-eng --test audit_provision` → 22 passed; 0 failed**。 |
| E3 | `crates/aero-cli/src/main.rs:434`（audit-provision-check 命令） | ⚠️ **Verified（行号漂移）**。命令 `AuditProvisionCheck_` 实际在 :488-497（`c!` 宏，name "audit-provision-check"，读 `DATABASE_URL`→`AERO__DATABASE__URL` → `aero_eng::audit_provision::run(&url).await`）；:434 附近是 relay-probe 臂。:316 cmds 串已含 `audit-provision-check`。 |
| E4 | `crates/aero-server/src/bin/main.rs:248-267`（relay boot gate：仅 presence-gate，无 verdict 咨询） | ✅ **Verified（核心缺口成立）**。:245-269 B5-2 段：注释 :247-250 原文 "Presence-gated on AERO_AUDIT_TOKEN_ENDPOINT; … booting before the 0239 table lands **degrades to logged claim errors, not a crash**"（F13）；`RelayConfig::from_env()` :251 → `Ok(Some)` → PgOutboxRepo + AuditClient + `tracker.spawn(relay.spawn(...))` :252-262；`Ok(None)` → 跳过；`Err` → boot error fail-loud :263-267。**全程零 `aero_eng::audit_provision` 引用、零 readyz/health 咨询**——verdict 只经手动 CLI 可达。 |
| E5 | `scripts/test-integration.sh:143-148`（37/37 pin 材料加载 + guard 自测） | ✅ **Verified**。:143-149 "B5 acceptance gate (G6 总装门): load the 37/37 contract-pin material (scripts/b5-pin.sh) and self-test the guard first"——`source …/b5-pin.sh` + `bash …/test-b5-pin-guard.sh`（纯 bash，先于 DB 工作）。 |
| E6 | `scripts/test-integration.sh:30-62`（T11/PRIORITY/PROVISION throwaway DBs） | ✅ **Verified**。:48-50 `T11_DRILL_DB` / `PRIORITY_DRILL_DB` / `AUDIT_PROVISION_DB`（`aero_*_$$` 命名）+ :62-76 `assert_disposable_db_name` 逐名校验。 |
| E7 | `scripts/test-integration.sh:245-288`（provision leg B） | ✅ **Verified**。:244-298 leg B：help-grep 检测命令（防 "unknown command" 非零误读为 PASS）→ B1（throwaway 库 migrate → exit 0 + `verdict: consistent`）→ B2（`run_psql` 直插 1 行 v1 audit 行 `destination='audit'` → 非零 + `verdict: fail-closed` + `no audit:event:write grant issued`）→ drop → `b5_check "audit-provision-check" "PASS"`；else 分支 :297-298 `SKIP (B5-4 audit-provision-check not landed)`。另 :368-380 leg A（T-11 DB 上 relay 缺席 ⇒ 拒发 grant）与 :381-433 leg D/C（healthy + 0239 分布 + oldest-pending-age；`UPDATE … SET status = 3` 后 `dead=1`/`delivered=0`/dead 明细/fail-closed）。 |
| E8 | `migrations/0235_snaplink_commercial_control_plane.sql`（`snaplink_commercial_runtime.enabled` singleton——enforcement 开关） | ✅ **Verified**。:8-16 `snaplink_commercial_runtime(singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton), enabled BOOLEAN NOT NULL DEFAULT FALSE)`，:14-16 恒 1 行 seed（`ON CONFLICT DO NOTHING`）；:24 `snaplink_commercial_bindings.enabled`。Q0 谓词的事实源（E1）。 |
| E9 | `docs/proposals/audit-contract-batch-aero-im.md`（B5-4 seam；registry 本体在 IdP 仓 [PROPOSED]） | ✅ **Verified（as proposed）**。15 行门摘要：B5-4 = "本仓库交付配给验证 seam（`aero-cli audit-provision-check`，[PROPOSED]）+ fail-closed（boot 门已验证存在；readyz 不翻转）；registry 本体在 IdP 仓"；:13 列出 8 处不可验证/[PROPOSED]（含 "37/37" 测试清单）；:15 "37/37 需先把契约测试清单钉入 test-integration.sh"。 |

### 1.1 对 direction 问题陈述的勘误/钉化（evidence-backed）

- **「nothing in-process enforces it」成立**（E4）：verdict 仅 CLI 可达；boot 只查 `RelayConfig::from_env` presence。本 direction 的核心缺口 = 把 `aero_eng::audit_provision` 的 verdict 接进 boot（**aero-server 已依赖 aero-eng**——`crates/aero-server/Cargo.toml:30` `aero-eng.workspace = true`，零新依赖；DB URL 在 `cfg.database`，boot 处可及）。
- **「readyz is explicitly not consulted」是钉死的约束，不是缺口**：`crates/aero-server/src/routes/health.rs` `probe_commercial` :69-79（runtime 缺席 → "disabled"；`runtime.ready()` 2s 超时 projection 式，注释 "A reachable central service is intentionally not a probe dependency"）+ `health_ready` deps_ok 只接受 `"ok" | "disabled"` :104/:186——**relay 缺席/不健康永不翻 readyz**（sibling aero-cli B5-4 spec AC3 已钉回归）。因此 AC1 的 "verdict consulted at boot **or via readiness**" 两选项里，**boot 时 fail-loud 是唯一不与 readyz 钉冲突的读法**——"via readiness" 会让 readyz 翻 fail，违反既有钉（§7 决策点 D1）。
- **「count is not verifiable in-repo」已过时（工作树校正）**：`scripts/b5-pin.sh` 已含 **37 槽 = 15 执行 + 22 `[PROPOSED]` 占位**，`assert_b5_contract_pin` 强校验恰 37/无重复/无 malformed/非空转/每执行槽有 `B5-CHECK <name>: PASS|SKIP` 证据行；guard 自测 `scripts/test-b5-pin-guard.sh` **实跑全绿**；harness 末尾打印 `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`。**剩余钉入 = 22 个 [PROPOSED] 占位在仓外契约文本落地后 one-touch 替换为真实清单**（proposal :13/:15），本 direction 钉「15 执行槽全绿 + guard + 37/37 报告 + [PROPOSED] 显式保留」。
- **行号漂移**：E3 `main.rs:434` → 命令在 :488-497。
- **B5-3 已随工作树落地**：`crates/aero-audit-connector/src/pg.rs:105` claim SQL 已含 `ORDER BY candidate.priority DESC, …`；priority drill 车道常量已与 `aero_ai::governance` 调和（drill :55-62 doc 引用 `GOVERNANCE_PRIORITY_BACKLOG`=10 / `GOVERNANCE_PRIORITY_MODERATION`=100，不再 [PROPOSED] 100/200）→ `moderation-priority-drill` 槽可翻 PASS（不属本 direction 验收，但 37/37 全绿依赖它）。
- **migration 尾号已过 0239**：`migrations/` 现有 `0239_audit_governance_outbox.sql` + `0240_audit_governance_due_prio_idx.sql` + `0241_governance_reconcile.sql`（均 untracked）；harness 的 0239 文件门 `[ -f migrations/0239_audit_governance_outbox.sql ]` 仍是各 leg 的开关（E7）。

## 2. Verified current state

```
seam（已在，untracked ⚠️）        crates/aero-eng/src/audit_provision.rs（E1，22/22 单测实跑绿）
                                  crates/aero-cli/src/main.rs:488-497 命令 + :316 cmds 串（E3）
                                  harness legs A/B1/B2/C/D 已接线（E7）；b5_check "audit-provision-check" 槽已复用

verdict 语义（已在）              Q0 DB 开关+bindings 谓词（E1）；三态矩阵 dead 优先；报告行全部 audit-provision-check: 前缀

gap（本 direction 闭合）          aero-server boot（main.rs:245-269）：RelayConfig::from_env presence-only
                                  └─ 无 verdict 咨询、无 refuse-to-start、readyz 不咨询（E4 + §1.1 钉）
                                  37/37：37 槽（15 执行 + 22 [PROPOSED]），guard + 自测 + 报告已在（§1.1 校正）
                                  └─ 剩余 = [PROPOSED] 占位 one-touch 替换 seam（仓外契约文本）

aero-ai 模块面（零改动，钉死）    crates/aero-ai/src/governance.rs（映射权威，6/6 单测实跑绿）
                                  crates/aero-ai/src/worker/mod.rs:372-404 handle_moderate（:401 LOCAL_ACTION_MODERATED
                                  → soft_delete_outboxed_system → audit_events → 0236/0239 触发器 → outbox 行
                                  = gate Q1/Q3 评估的行种群）
                                  跨模块文本 pin：priority-drill :55-62 doc ↔ governance.rs 常量；0239 触发器字面量旁 cross-slice pin 注释

readyz 语义（钉，不动）           health.rs probe_commercial/health_ready（"ok"|"disabled" 才 ready；relay 缺席永不翻）

依赖可行性（已验证）              aero-server Cargo.toml:30 aero-eng.workspace = true；cfg.database 含 DB URL（main.rs:37 区）
```

**本 direction 关闭的剩余项**：① boot 接入 fail-closed verdict（refuse-to-start on `Verdict::FailClosed`，Consistent/Healthy 放行，无 endpoint 路径零改动）——verdict 咨询从「仅手动 CLI」变为「boot/ops 时强制」；② 37/37 的 in-repo 可验证面钉死（15 执行槽全绿 + guard 自测 + harness 37/37 报告 + [PROPOSED] seam），并随 batch 提交。

## 3. Scope

**In scope（module `crates/aero-ai/src` 的 B5-4 切片）**：
- 钉死 gate 评估的行种群契约（R1）：aero-ai 的 moderation-finalize 生产者（token-keyed、R-D2 负钉）——**零生产代码改动**，行种群 = `message.moderated` audit 行 → outbox 行（v1 + 0239 admin 类）。
- 钉死 verdict 契约（R2）：dead 终态永不折入 delivered、relay off + undelivered → FailClosed、空库 → Consistent exit 0——单元 oracle = aero-eng 22/22（实跑绿）。
- boot 门接线（R3，落地在 sibling aero-server `main.rs`）：relay spawn 前咨询 verdict——`FailClosed` → boot Err fail-loud（消息携带 verdict 原文）；`Consistent`/`Healthy` → 放行；`AERO_AUDIT_TOKEN_ENDPOINT` 缺席路径**字节级不变**（relay 缺席照常起服务）；readyz **零改动**。判定映射做成纯函数（`decide_relay_boot`，落 `aero-eng::audit_provision`，无服务依赖可单测）。
- 37/37 钉入闭环（R4）：15 执行槽全绿（含 aero-ai 名下 `audit_governance::` / `moderation_finalize_outbox_parity`）、guard 自测、harness 37/37 报告、[PROPOSED] 占位显式保留 + one-touch 替换 seam。
- 提交 untracked 的 batch 产物（governance.rs、audit_provision.rs + tests、b5-pin/test-integration 改动等，§7 最高风险）。

**Out of scope（并行切片 / 红线——勿在本 direction 建造）**：
- 配给本体（grant 动作 / IdP scope registry）→ 仓外（proposal :11）；本 direction 只做**验证 + 非零拒绝**，不签发任何东西。
- auth 侧 `assert_audit_scope_provisioned` 拒绝点 / durable 心跳 / 0240 迁移 → sibling `2026-08-07-aero-auth-b5-4-relay-provisioning-gate.req.md`（本 direction 的 boot 门与心跳 seam 互补不重叠：boot 门 = 启动期 fail-closed 预检；心跳 = 运行期 "relay works" 证明）。
- 0239/0240/0241 DDL、claim priority DESC、L1 聚合、connector 403/422→dead 状态机 → B5-1/B5-2/B5-3（已在树，只作 oracle/回归保持）。
- readyz 语义任何改动：**禁止**（§1.1 钉）。
- `AiWorker` / `messages.rs` / `message_reports.rs` 生产者代码、`governance.rs` 逻辑、connector 生产代码：**禁止**（R1/R5）。
- 完整 JWT 签名验证（JWKS）→ [PROPOSED] seam；本 seam 的 Q0-Q5 纯 DB 谓词已足够（acceptance 只要求检出未配给状态）。

## 4. Requirements

### R1 — Gate 评估的行种群契约（aero-ai 模块面，零代码改动）
- 唯一 admin 类本地 token = `LOCAL_ACTION_MODERATED = "message.moderated"`（governance.rs:49），全部生产者——`AiWorker::handle_moderate`（worker/mod.rs:372-404，:401 传 token）、`ImService::moderate_delete`（messages.rs，含 `moderation_bot` 路径）、`message_reports::review_authorized`（actor=Some(reviewer)）——经 `soft_delete_outboxed_system` 同事务写 audit 行 + outbox 行（0236 v1 / 0239 admin 类 status=0）。
- 映射**仅键于 token**（`governance_lane_for` :86-99，无调用者身份输入）；未知 token → `None` pass-through，永不 raise/block（`unknown_local_token_passes_through_unmapped` 钉）。
- R-D2 负钉保持：`message.deleted` 绝不进 admin 车道（`user_delete_token_stays_out_of_admin_lane`）。
- 这些行是 gate Q1（v1 `destination='audit'`）/Q3（0239 四桶）评估的种群——**gate 的 fail-closed 语义直接作用于本模块产出的行**。
- 保持绿：`cargo test -p aero-ai governance::` 6/6（实跑已验证）。

### R2 — Verdict 契约（gate 语义，单元钉死）
`aero_eng::audit_provision::verdict`（:109-122）三态矩阵，dead 优先：

| 输入 | Verdict | 退出码 |
|---|---|---|
| 0239 dead > 0（relay 开或关） | `FailClosed("N dead row(s): … dead is never counted delivered")` | 1 |
| relay 不健康（`!enabled ∨ bindings=0`）∧ 任何 undelivered（v1 pending+claimed + 0239 pending+claimed）> 0 | `FailClosed("relay disabled (bindings=M) with K undelivered audit row(s); no audit:event:write grant issued")` | 1 |
| relay 不健康 ∧ 零 undelivered | `Consistent` | 0 |
| relay 健康（enabled ∧ bindings>0）∧ dead=0 | `Healthy`（pending 积压属正常） | 0 |
| 任何操作路径（URL 坏/psql 不可用/DB 不可达/超时/表缺失） | `Outcome::error`（永不为 0） | 1 |

- 报告行全部 `audit-provision-check:` 前缀（format_report :125-166）：`relay: enabled=… bindings=…`、`v1-outbox: pending=… claimed=… delivered=…`、`outbox-0239: table=… pending=… claimed=… delivered=… dead=…`、`oldest-pending-age: …s`、`dead: <event_id> <last_error>`、`verdict: …`。
- 保持绿：`cargo test -p aero-eng --test audit_provision` 22/22（实跑已验证），其中 `verdict_dead_is_fail_closed_even_with_relay_on` / `verdict_dead_priority_over_relay_disabled` / `verdict_relay_off_with_undelivered_is_fail_closed` / `verdict_relay_off_empty_is_consistent` 是本 direction 的语义钉。

### R3 — Boot 门：relay spawn 前咨询 verdict（fail-closed，落地 sibling aero-server）
- 新纯函数（落 `crates/aero-eng/src/audit_provision.rs` 或同 crate 新模块，无服务依赖）：
  `pub fn decide_relay_boot(v: &Verdict) -> Result<(), String>` —— `FailClosed(reason)` → `Err(reason)`；`Consistent | Healthy` → `Ok(())`。**单元可测，无需 PG/Redis/NATS**。
- `crates/aero-server/src/bin/main.rs:251` `RelayConfig::from_env()` 的 `Ok(Some(relay_cfg))` 分支、`tracker.spawn(relay.spawn(...))` **之前**：以 `cfg.database` 的 DB URL 调 `aero_eng::audit_provision::run(&url).await`（30s 超时、local/container psql 双模式已内置，E1），`decide_relay_boot` 映射——`Err` → `return Err(error).context("audit connector relay: provisioning gate fail-closed")`（boot 中止，消息含 verdict 原文，stderr 可 grep）。
- `Ok(None)` 路径（无 `AERO_AUDIT_TOKEN_ENDPOINT`）**零改动**：relay 缺席照常起服务（既有行为，AC1-①）；`Err` 分支（stray `AERO_AUDIT_*`）既有 fail-loud 保持。
- **readyz/health 零改动**（§1.1 钉；`probe_commercial`/`health_ready` 一行不动）。
- 语义边界（与 F13 降级共存）：DB 开关关 + 零 undelivered → `Consistent` → 照常 boot（relay 空闲，0239 未迁移时的 logged-claim-error 降级不变）；**只有「relay 配置了但配给不成立且有无投递行」才 fail-loud**——即 AC1-② 的 "DB switch off/undelivered rows pending" 组合。
- 门是**启动期一次性预检**，不进每事件路径；不做运行期重检（心跳 = sibling aero-auth slice）。

### R4 — 37/37 契约清单钉入（in-repo 可验证面）
- `scripts/b5-pin.sh`：37 槽 = **15 执行 + 22 `[PROPOSED]`**（占位内容不臆造，proposal :13）；`assert_b5_contract_pin` 守卫强校验恰 37/无重复/无 malformed/≥1 执行槽/每执行槽有 `B5-CHECK <name>: PASS|SKIP` 证据行（SKIP-with-reason 计数为已处理）。
- 15 执行槽全绿（本 direction 验收时）：`rolling_upgrade_fences…`、`migration_0192…`、`migration_0228…`、`migration_0233…`、`migration_0237…`、`message_quota_and_snaplink_outboxes_are_transactional`、`scim_inactive…`、`audit_governance::`、`moderation_finalize_outbox_parity`、`a3-relay-drill`、`t11-fail-closed`、`moderation-priority-drill`（依赖 B5-3 claim DESC，已在树）、`notification-fanout`、`relay-mock-probe`、`audit-provision-check`。
- guard 自测 `bash scripts/test-b5-pin-guard.sh` 全绿（实跑已验证：positive + count/dup/malformed/vacuous/missing-verdict/skip-db-create 各负例）。
- harness 末尾打印 `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`（非零即 exit 1）。
- `[PROPOSED]` 槽 = 仓外 v2 契约文本落地后 one-touch 替换的真实契约测试名（seam 已就位；替换后 guard 自动开始要求其 verdict 证据）——本 direction 不虚构契约项。

### R5 — 约束
- **aero-ai 生产代码零改动**（worker/mod.rs、governance.rs、lib.rs 逻辑不动）；governance.rs 随本 direction 提交（untracked，§7）。
- **aero-server 改动最小化**：只动 `main.rs` relay boot 分支（R3）；health.rs/readyz 一行不动；`aero-eng.workspace = true` 已存在（Cargo.toml:30），零新依赖。
- **connector/`aero-audit-connector` 零改动**；drill 语义零改动（legs 已接线，E7）。
- 提交前门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 · `bash scripts/test-b5-pin-guard.sh` 全绿。

## 5. Acceptance checks（direction 四条原样保留，逐条 testable）

### AC1 — Boot without AERO_AUDIT_TOKEN_ENDPOINT stays up with relay absent（既有行为）AND boot with relay config but DB switch off/undelivered rows pending fails loud with the fail-closed verdict（proposed: verdict consulted at boot, not only a manual CLI）

- **Oracle A（决策映射纯函数，无服务）**：`cargo test -p aero-eng decide_relay_boot`——`FailClosed(_)` → `Err`（消息含 verdict 原文）；`Consistent`/`Healthy` → `Ok`。用例矩阵镜像 verdict 矩阵（dead / relay-off+undelivered / relay-off+empty / healthy+backlog 各一）。
- **Oracle B（relay 缺席 boot 不回归）**：`main.rs` `Ok(None)` 路径零 diff（`git diff crates/aero-server/src/bin/main.rs` 只含 R3 的 `Ok(Some)` 分支改动）；冒烟：无 `AERO_AUDIT_*` env 起 server → 正常起服务（日志无 relay 段，无新错误路径）。
- **Oracle C（fail-loud 集成 drill，新命名段）**：test-integration.sh 新增 `relay-boot-provisioning-gate` 段（复用 `AUDIT_PROVISION_DB` throwaway 流程）：已迁移库上 seed 1 行 v1 undelivered audit 行（B2 同款 seed）→ `AERO_AUDIT_TOKEN_ENDPOINT` 指向 loopback（配置齐 AERO_AUDIT_* 其余项）→ 编译后 server 二进制 boot → **非零退出**且 stderr grep `fail-closed` + `no audit:event:write grant issued`；同一库清空 undelivered（`DELETE`）→ boot → 起服务并打印 `audit connector relay enabled`（短命跑完 `pkill aero-server`，AGENTS.md §4.3 前台/setsid 活法）。全服务栈 drill 若在 harness 环境不可行（PG/Redis/NATS 齐全，按 AGENTS.md 应可行），Oracle A+B 为最低契约面（§7 D2）。

### AC2 — T-11: audit-provision-check legs A1/B1/B2 already in test-integration.sh stay green（empty DB → exit 0 consistent; one undelivered v1 audit row + relay off → non-zero with 'no audit:event:write grant issued'）

- **Oracle**：`bash scripts/test-integration.sh`（PG 可用）→ B5 日志含 `B5-CHECK audit-provision-check: PASS`（:296），且：
  - leg B1（:262-276）：空迁移库 → `cargo run -p aero-cli -- audit-provision-check` **exit 0** + grep `verdict: consistent`；
  - leg B2（:278-295）：1 行 undelivered v1 audit 行（`delivery_id='audit:b5-leg-b'`、`destination='audit'`）→ **非零** + grep `verdict: fail-closed` + `no audit:event:write grant issued`；
  - leg A（:368-380）：T-11 DB（relay 缺席）→ 非零（拒发 grant）。
  - 反向断言（防 stale/no-op 命令）：`cargo run -p aero-cli -- help` grep `audit-provision-check`（legs 自带 help-grep 守卫）。

### AC3 — 37/37: the b5-pin.sh list is fully populated with the 37 contract tests, guard self-test passes, and the harness reports 37/37 green in CI（in-repo 可验证读法：15 执行槽全绿 + 22 [PROPOSED] 显式占位 + guard + 报告）

- **Oracle A（guard 自测）**：`bash scripts/test-b5-pin-guard.sh` → 全绿（positive + 各负例）。
- **Oracle B（harness 报告）**：`bash scripts/test-integration.sh` 末尾输出 `B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`（`assert_b5_contract_pin` 非零即 exit 1）。
- **Oracle C（aero-ai 名下槽翻转）**：B5 日志含 `B5-CHECK audit_governance::: PASS` 与 `B5-CHECK moderation_finalize_outbox_parity: PASS`（0239 文件门内 `run_migrated_integration` 空过滤守卫执行，非 `SKIP (0239 not landed)`）。
- **Oracle D（[PROPOSED] seam）**：`grep -c 'contract-test-[0-9]*\[PROPOSED\]' scripts/b5-pin.sh` = **22**（精确模式；裸 `[PROPOSED]` grep 会命中注释行得 29，勿用）；占位格式 `contract-test-NN[PROPOSED]`（NN=01..22）逐一在列（内容不臆造）。

### AC4 — Dead rows（status=3）reported as terminal fail-closed state in Q3 — never folded into delivered — asserted in the provision gate

- **Oracle A（单元钉，无 DB）**：`cargo test -p aero-eng --test audit_provision` 22/22——`verdict_dead_is_fail_closed_even_with_relay_on`（relay 健康 + dead=1 → FailClosed，reason 含 "dead" + "never counted delivered"）、`verdict_dead_priority_over_relay_disabled`（dead 优先于 relay-off 判定）、`report_lists_dead_rows_separately_from_delivered`（`delivered=0` 与 `dead=1` 分行、dead 明细行存在）。
- **Oracle B（集成 leg C，已在 harness）**：T-11 DB 上 `UPDATE audit_governance_outbox SET status = 3, last_error = '403 provisioning refusal (drill)'`（:416-419）→ `audit-provision-check` **非零** + grep `dead=1` + `delivered=0` + `audit-provision-check: dead:` + `verdict: fail-closed`（:420-432）→ `b5_check "audit-provision-check" "PASS"`。
- **Oracle C（Q3 桶格式）**：leg D 断言 `outbox-0239: table=audit_governance_outbox pending=[0-9]+ claimed=0 delivered=0 dead=0` + `oldest-pending-age:`（:393-409）；dead 行自带 `event_id|last_error(≤120)` 明细（Q5，audit_provision.rs:54-55）。

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| verdict 矩阵 + dead 优先 + 报告（AC2/AC4 Oracle A） | `crates/aero-eng/tests/audit_provision.rs`（22 项，已在） | `cargo test -p aero-eng --test audit_provision`（无 DB；实跑 22/22 绿） |
| `decide_relay_boot` 决策映射（AC1 Oracle A） | `crates/aero-eng/src/audit_provision.rs`（或同 crate 新模块）+ tests | `cargo test -p aero-eng`（无 DB） |
| boot fail-loud drill（AC1 Oracle C） | `scripts/test-integration.sh` 新命名段 `relay-boot-provisioning-gate`（复用 AUDIT_PROVISION_DB + help-grep 守卫先例） | 编译后 server 二进制 + throwaway 已迁移库 + loopback endpoint；短命跑完 pkill |
| legs A/B1/B2/C/D（AC2/AC4 Oracle B/C） | `scripts/test-integration.sh:244-298,368-433`（已在，零改动） | `bash scripts/test-integration.sh`（throwaway 库全链迁移） |
| 37/37 guard 自测 + 报告（AC3 Oracle A/B/D） | `scripts/b5-pin.sh` + `scripts/test-b5-pin-guard.sh` + harness 末尾 `assert_b5_contract_pin`（已在） | `bash scripts/test-b5-pin-guard.sh`（实跑全绿）+ harness 报告行 grep |
| aero-ai 名下槽（AC3 Oracle C） | `crates/aero-storage/src/audit_governance.rs` db_tests（`moderation_finalize_outbox_parity` :211 等）经 `run_migrated_integration` | `--test-threads=1` + 空过滤守卫 |
| 映射权威保持（R1） | `crates/aero-ai/src/governance.rs`（6 项单测，已在） | `cargo test -p aero-ai governance::`（无 DB；实跑 6/6 绿） |
| 回归（boot 路径 / readyz / 命令注册） | `cargo test --workspace --lib` + health.rs 既有 `readiness_decision_*` + `aero-eng help` 冒烟 | cargo + shell |

## 7. Risks / [PROPOSED] / 决策点

- **最高风险：全 batch 产物 untracked**。governance.rs、audit_provision.rs(+tests)、aero-audit-connector/、audit_governance.rs、drills、b5-pin/test-integration 改动、migrations 0239-0241 均未提交——任何 `git reset --hard master`（AGENTS.md §4.1）都会抹掉本 direction 全部验收对象。**提交是 AC1-AC4 全部 oracle 成立的前提**（与 sibling aero-ai B5-1 spec §7 同判）。
- **D1 — AC1 的 "boot or readiness" 二选一钉为 boot fail-loud**：readyz 不翻转是既有钉（health.rs 注释 + sibling aero-cli B5-4 spec AC3）——"via readiness" 会翻 readyz，违反钉。boot 时一次性预检 + fail-loud 是唯一不冲突读法；运行期重检（心跳）归 sibling aero-auth slice，本 direction 不做。
- **D2 — boot drill 的全服务栈依赖**：Oracle C 需 PG+Redis+NATS 齐全（harness 环境按 AGENTS.md 具备；AGENTS.md §4.3 server 只能前台/setsid 跑 + pkill）。若某环境跑不动，最低契约面 = Oracle A（纯函数）+ Oracle B（零 diff 断言）+ AC2 legs（CLI 级 verdict 即 boot 咨询的同一 verdict 源）——Oracle C 是完整证据，A+B 是可执行底线。
- **D3 — psql 依赖 at boot**：`run` 需 psql 或 docker exec（`AERO_PSQL_MODE`/`AERO_POSTGRES_CONTAINER`，PsqlRunner 双模式 + auto 回退，E1）；boot 环境若无 psql 无容器 → `Outcome::error` fail-loud（配给无法验证即拒绝——符合 fail-closed 语义，不是 bug；消息提示 env）。
- **D4 — 30s QUERY_TIMEOUT 于 boot 路径**：一次性预检可接受（不是每事件路径）；超时 = fail-loud（`Outcome::error`）。
- **37/37 的 22 个 [PROPOSED] 槽内容在仓外**（proposal :13）：本 direction 钉「37 槽具名 + 15 执行槽全绿 + guard + 报告 + 占位显式」，**不臆造契约项**；契约文本落仓后 one-touch 替换（R4 seam 已就位）。"37" 计数本身也是契约来源，guard 钉为常量。
- **`moderation-priority-drill` PASS 依赖 B5-3**：claim `ORDER BY priority DESC` 已在树（pg.rs:105）→ 可翻 PASS；若 B5-3 回退，该槽按 drill 语义 FAIL 红（诚实 G6 信号），非本 direction 回归。
- **aero-server 改动面**：只动 `main.rs` relay boot 分支；`Ok(None)` 路径零 diff（Oracle B 断言）；readyz/health/connector 零改动。
- **与 sibling aero-auth B5-4 的重叠边界**：auth spec 的 gate 拒绝点/心跳是运行期 seam，本 direction 的 boot 门是启动期预检——互补不重叠；集成时 auth spec 的 CLI 段落（若保留）与本 seam 共用同一 `audit-provision-check` 命令/退出码语义（已在树，E3/E7）。

## 8. Sequencing

1. **提交 batch 产物**（前置，§7 最高风险）：governance.rs + audit_provision(+tests) + b5-pin/test-integration 改动 + 其余 untracked batch 文件（按 batch 提交规范归组）。
2. **快速 oracle**：`cargo test -p aero-ai governance::`（6/6）→ `cargo test -p aero-eng --test audit_provision`（22/22）→ `bash scripts/test-b5-pin-guard.sh`（全绿）——全部实跑已验证，作为基线。
3. **R3 落地**：`decide_relay_boot` 纯函数 + 单测（AC1 Oracle A）→ `main.rs` `Ok(Some)` 分支接线（boot 前 `run` + 映射，fail-loud context 消息）→ `git diff` 确认 `Ok(None)` 路径零改动。
4. **boot drill**：test-integration.sh 新命名段 `relay-boot-provisioning-gate`（AC1 Oracle C，复用 AUDIT_PROVISION_DB + seed 先例）。
5. **全链 integration**：`cargo build` → `bash scripts/test-integration.sh`——legs A/B1/B2/C/D 全 PASS、15 执行槽 verdict 行齐、`B5 contract pin: 37/37 (15 executed, 22 [PROPOSED]): PASS`。
6. **门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 · no-touch 守卫（`git diff --stat` 不含 readyz/health、connector、aero-ai 生产代码）。
