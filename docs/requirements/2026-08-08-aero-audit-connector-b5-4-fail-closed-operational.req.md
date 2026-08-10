# Requirements Spec — aero-audit-connector B5-4：fail-closed 运行化（dead/pending 采样成 gauge + 配给门自动反馈，不再只靠手动 CLI）

- **Module (analysis root)**: `crates/aero-audit-connector` — B5-2 leased-relay connector（`relay.rs` 状态机、`pg.rs` 0239 表 SQL、`fake.rs` 测试替身、`stub.rs` HTTP 替身、`aero-audit-t11-drill`/`aero-audit-priority-drill`/`aero-audit-relay-drill` 三个 drill bin）
- **Direction**: "Make B5-4 fail-closed operational: relay dead/pending state as sampled metrics + automatic provisioning gate feedback, not a manual CLI"（value 8 / risk_reduction 9 / effort 6 / confidence 8）
- **Source analysis**: `docs/auto/analyses/crates-aero-audit-connector-40d338d1.json`（direction #2）
- **Campaign**: `aero-im-b5-outbox-relay`；B5-4 seam 契约锚点 `docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md` + `docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`（CLI seam 本体，已落地）
- **Sibling specs（同契约不同切片，勿撞）**: `2026-08-08-aero-ai-b5-4-relay-boot-provisioning-gate.req.md`（**requirements 态未落地**——`decide_relay_boot` 不在树，见 §1.1；本 direction 与其互补不重叠）、`2026-08-08-aero-audit-connector-b5-3-claim-lane-carriage.req.md`（claim 车道，已落地）
- **Status**: Requirements（下述证据全部经源码 grep 核对，行号为核对时锚点可能漂移——**文件/符号**才是稳定 grep 锚点，AGENTS.md §0）
- **Verification date**: 2026-08-08

## 1. Evidence verification（direction 引用逐条核对）

| # | Cited evidence | Verification result |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs` — Verdict matrix（dead>0 或 relay-disabled+undelivered → FailClosed；CLI-only 执行路径） | ✅ **Verified**（718 行）。`verdict()` :125-138 三态矩阵 **dead 优先**：`g0239.dead > 0` → `FailClosed("N dead row(s): the audit:event:write grant was refused (403/provisioning); dead is never counted delivered")` :128-134；relay 不健康（`!enabled ∨ bindings=0`）∧ undelivered>0 → `FailClosed("relay disabled (bindings=M) with K undelivered audit row(s); no audit:event:write grant issued")` :135-141；relay 不健康 ∧ 零 undelivered → `Consistent`；健康 → `Healthy`。**CLI-only 执行路径成立**：`crates/aero-cli/src/main.rs:489-504`（`AuditProvisionCheck_` 臂，`audit-provision-check [--priority]` → `aero_eng::audit_provision::run(&url)` / `run_priority`）；`aero_eng` 无服务依赖（`Cargo.toml`：aero-common/tokio/serde_json/anyhow/async-trait/time/serde/toml，**无 sqlx**，psql 走子进程）。22 个单测在 `crates/aero-eng/tests/audit_provision.rs`（`verdict_dead_is_fail_closed_even_with_relay_on` / `verdict_dead_priority_over_relay_disabled` 等）。 |
| E2 | `crates/aero-server/src/bin/boot/metrics_tasks.rs` — gauge samplers（DB pool/WHIP/AI DLQ/NATS backlog；无 audit outbox gauge） | ✅ **Verified**（440 行）。`spawn_all` 内 sampler：DB pool+WHIP 15s、index-size（`INDEX_SIZE_BYTES`，60s，`AERO_INDEX_SIZE_SAMPLE_SECS`）、PG stats（`sample_pg_health`，60s，`AERO_PG_STATS_SAMPLE_SECS`）、AI DLQ 30s（`AI_DEAD_LETTER_QUEUE_SIZE` + `kind` label，:135-166）、NATS backlog 30s（`NATS_CONSUMER_PENDING_MESSAGES` + stream/consumer labels，:168-214）、call/stream 心跳、REMB tick。**全仓 `rg -ni audit` 于 `crates/aero-common/src/metrics.rs` names 零命中**——无 audit outbox gauge 的缺口成立。sampler 模式：`MissedTickBehavior::Skip` + 共享 cancel token + query Err → `tracing::warn!` 留旧值（**永不改状态**，AGENTS §2「纯观察设 Prometheus gauge，永不改状态」）。 |
| E3 | `crates/aero-audit-connector/src/relay.rs` — deliver_claim: DeliveryError::Forbidden → mark_dead immediately；terminal，无 feedback path | ✅ **Verified**（454 行）。`deliver_claim`：`Err(DeliveryError::Forbidden)` → `repo.mark_dead(event_id, token, attempts, "audit sink rejected the service identity (HTTP 403)")` 立即死（注释「HTTP 403 is fail-closed immediate death (T-11)… NOT `is_dead_at`」）；fence 丢失只 `warn!`。**无任何下游消费者/反馈路径**——dead 行累积后除 CLI 外无人读。 |
| E4 | `migrations/0239_audit_governance_outbox.sql` — enqueue gate = runtime.enabled only（never relay health） | ✅ **Verified**（140 行）。`aero_enqueue_governance_audit()` 触发器：Gate 1 = `SELECT runtime.enabled FROM snaplink_commercial_runtime runtime WHERE runtime.singleton`（:64-69，fail-open `RETURN NEW`）；Gate 2 = `aero_snaplink_binding_for_workspace`（:83-89，fail-closed RAISE）；token-keyed 仅 `message.moderated`（:73-76）。**全程零 relay-health/dead-row 咨询**——enqueue 门不感知 relay 死活。0239 表 = `status INTEGER CHECK (status IN (0,1,2,3))` + `last_error TEXT` + `available_at/created_at`（dead 行时间戳可派生）。 |
| E5 | `crates/aero-server/src/bin/main.rs:251-266` — relay spawn，presence-gated on AERO_AUDIT_TOKEN_ENDPOINT；no health hook | ✅ **Verified（行号精确）**。relay 段 :245-268：注释 :247-250「Presence-gated on AERO_AUDIT_TOKEN_ENDPOINT; … booting before the 0239 table lands degrades to logged claim errors, not a crash」（F13）；`RelayConfig::from_env()` :251 → `Ok(Some)` → PgOutboxRepo + AuditClient + `tracker.spawn(relay.spawn(ai_shutdown.clone()))` :260；`Ok(None)` → 跳过；`Err` → fail-loud。`spawn_metrics_tasks` :270。**main.rs 全文件 `rg "audit_provision|provisioning"` 零命中**——无 verdict/health 咨询。presence gate 本体：`config.rs:49-63`（`AERO_AUDIT_TOKEN_ENDPOINT` 缺席且无其他 `AERO_AUDIT_*` → `Ok(None)`）。 |

### 1.1 对 direction 问题陈述的钉化（evidence-backed）

- **「fail-closed 仅靠手动 CLI 强制」成立（截至本工作树）**：sibling `2026-08-08-aero-ai-b5-4-relay-boot-provisioning-gate.req.md` 规划的 boot 门（`decide_relay_boot` + main.rs boot fail-loud）**只有 requirements 文档，未落地**——`rg decide_relay_boot` 全仓零命中（除该 req 文档自身），main.rs relay 分支无 verdict 咨询。本 direction 的「自动反馈」不与任何已落地物冲突。
- **「dead 行累积不可见」成立**：`metrics_tasks.rs` sampler 清单（E2）无 audit 面；`aero-common::metrics::names` 无 audit 名；`crates/aero-server/src/metrics.rs` 的 sampler 函数（`sample_index_sizes` :151 / `sample_pg_health` :202）无 audit 对应物。
- **「0239 enqueue 门只看 runtime.enabled」成立**（E4）；但**改触发器 = 新迁移 + 生产者侧改动，不在本 direction**（§3 红线：acceptance 只要求 gauge + 门反馈 + 测试，不要求 enqueue 门读 relay 健康——那是 B5-1 生产者切片的边界）。
- **「403 → mark_dead 无消费者」成立**（E3）；dead 行自带 `last_error`（403 文案）与 `available_at`/`created_at`——「last-403 时间戳」可派生（`max(available_at)` of status=3），但 direction 的 health signal 用「e.g.」二选一，**dead-count 是唯一必需信号**（verdict 规则就是 dead>0，见 §7 D1）。
- **依赖可行性（已验证）**：`crates/aero-server/Cargo.toml` :29 `aero-common`、:30 `aero-eng`、:36 `aero-audit-connector`（均为 `workspace = true`）——sampler 所在 aero-server 可直接用 `aero_audit_connector::pg::PgOutboxRepo` + `aero_eng::audit_provision::verdict`，**零新依赖**；`crates/aero-eng/Cargo.toml` 无 sqlx 无 connector 依赖，connector 加 `[dev-dependencies] aero-eng.workspace = true` **不成环**（connector 的 state_machine 测试需要）。
- **T-11 / priority drill 语义（回归面）**：`aero-audit-t11-drill.rs` 断言 `COUNT(status=0)==N`、`COUNT(status IN (1,2,3))==0`、`SUM(attempts)` 逐轮递增；`aero-audit-priority-drill.rs` 断言 moderation 首批 + drain-501 + parity-501——**sampler 若改状态（哪怕一行 UPDATE）即打爆这些计数**，天然回归守卫（§5 AC4）。

## 2. Verified current state

```
seam（已在）                     crates/aero-eng/src/audit_provision.rs（E1：verdict 矩阵 dead 优先；22/22 单测）
                                crates/aero-cli/src/main.rs:489-504（audit-provision-check 命令，CLI-only）
                                crates/aero-audit-connector/src/relay.rs:236-263（403 → mark_dead 立即死，E3）
                                migrations/0239（status 0/1/2/3 + last_error + 时间戳；enqueue 门仅 runtime.enabled，E4）
                                crates/aero-server/src/bin/main.rs:245-268（relay spawn presence-gated，E5）

gap（本 direction 闭合，全部 verified）
                                dead 行累积无 gauge：metrics_tasks.rs sampler 清单无 audit 面（E2）
                                verdict 只经手动 CLI：main.rs 无咨询、无运行期循环（E1+E5）
                                connector 无只读 bucket 采样面：OutboxRepo trait 只有
                                  reconcile/claim_due/settle/requeue/mark_dead（outbox.rs:69-112），全 mutation
                                无「403 → 门 FailClosed」的自动化测试链：state_machine.rs
                                  forbidden_dead_on_first_attempt（:242-266）只断言 FakeStatus::Dead，不断言门

可复用先例（已实跑）             sampler 模式：metrics_tasks.rs AI DLQ block（:135-166）+ aero-server/src/metrics.rs
                                  sample_pg_health（:202，只读、fail-open、warn 留旧值）
                                gauge 命名：aero-server/src/metrics.rs 局部 const（INDEX_SIZE_BYTES :88 等）
                                  + register_help（metrics_tasks.rs index-size block 内）
                                bucket SQL 先例：audit_provision.rs Q3_SQL :49（SELECT status, count(*)
                                  FROM {table} GROUP BY status ORDER BY status）+ parse_buckets :275
                                  （OutboxStatus::from_i32 fail-open，未知 status 忽略）
                                FakeStatus::code()（fake.rs:40-46）= 0..3 与 0239 数值一一对应
```

## 3. Scope

**In scope（本 direction 的交付物，全部围绕「dead/pending 采样 + 门反馈」）**：
- **R1** connector 只读 bucket 采样面：`OutboxRepo::status_buckets()`（trait + `PgOutboxRepo` + `FakeOutbox` 三处实现）。
- **R2** 运行期 gauge sampler：`sample_audit_outbox(&pool)`（落 `crates/aero-server/src/metrics.rs`，`sample_pg_health` 同款只读形态）+ `metrics_tasks.rs` 新 timer block（30s）+ gauge 名 const + `register_help`。
- **R3** 配给门自动反馈：sampler 循环内用采样快照（buckets + Q0 relay 谓词）组装 `AuditSnapshot` → `aero_eng::audit_provision::verdict()` → `FailClosed` 时 ERROR 日志（携带 verdict 原文，greppable）+ fail-closed indicator gauge——403 → dead 行 → 门自动 FailClosed，不依赖 CLI 执行。
- **R4** `crates/aero-audit-connector/tests/state_machine.rs` 新测试：403 → mark_dead → bucket 采样 → 门 verdict → FailClosed（connector 加 `aero-eng` dev-dependency，§1.1 已验证不成环）。
- **R5** 回归保持：T-11 / priority drill / 既有 state_machine 与 relay 单测 / aero-eng 22/22 全绿。

**Out of scope（红线——勿在本 direction 建造）**：
- **「自动翻转 `snaplink_commercial_runtime.enabled` 开关」（direction proposed #1）**：**否决**——翻转 enforcement 开关会同时禁用 v1 `snaplink_delivery_outbox`/usage 等无关商业 enforcement（Q0 谓词事实源 0235），副作用远超「配给 fail-closed」；acceptance 的 flip 对象是 **verdict**（"flips verdict to FailClosed"），不是开关（§7 D2）。
- **0239 enqueue 触发器改读 relay 健康**：生产者侧（B5-1 切片）语义，需新迁移；acceptance 不要求；enqueue 门的「fail-open runtime gate」是 A2 half(4) 已钉契约（`moderation_finalize_runtime_disabled_commits_1_plus_0`），动它即破钉。
- **CLI 改动**：`audit-provision-check` 保持现状（手动 oracle）；不加新命令/flag。
- **readyz / health 任何改动**：sibling B5-4 钉（`probe_commercial` 不翻 readyz），本 direction 零接触。
- **relay.rs / client.rs / claim/settle/requeue/mark_dead 状态机任何改动**：E3 的 403 → mark_dead 语义是 T-11 已钉契约，本 direction 只加**消费者**，不改生产者。
- **JWKS 签名验证（direction #3）**：另一 direction，勿混。
- **新迁移**：零。0239 表已含全部所需列（status/last_error/时间戳）。
- **aero-common 改动**：零（gauge 名走 aero-server/src/metrics.rs 局部 const 先例，INDEX_SIZE_BYTES :88 同款，不动 `names` 模块）。

## 4. Requirements

### R1 — 只读 bucket 采样面（`OutboxRepo::status_buckets`，connector 内）

- 新 trait 方法（`crates/aero-audit-connector/src/outbox.rs`，紧邻既有五方法）：
  `async fn status_buckets(&self) -> Result<StatusBuckets, Error>`，其中
  `StatusBuckets { enqueued: i64, claimed: i64, delivered: i64, dead: i64 }`
  （字段顺序/命名与 `aero_common::model::audit::OutboxStatus` 0/1/2/3 一一对应）。
- **只读契约**：纯 `SELECT … GROUP BY`，无 `FOR UPDATE`、无 `UPDATE`、无 `DELETE`、无 `last_error`/`attempts` 写入——AGENTS §2「永不改状态」硬规则。
- `PgOutboxRepo` 实现（`pg.rs`）：SQL 镜像 `audit_provision.rs` Q3_SQL :49（`SELECT status, count(*) FROM audit_governance_outbox GROUP BY status ORDER BY status`）；解析用 `OutboxStatus::from_i32` **fail-open**（未知 status 忽略，与 `parse_buckets` :275 同语义）；**0239 表缺席（pre-B5-1 boot）→ `Err(sqlx)`**（与 `claim_due` 今天的 F13 降级一致，由 sampler 侧 warn+留旧值消化，见 R2）。
- `FakeOutbox` 实现（`fake.rs`）：从内存 `BTreeMap` 按 `FakeStatus` 计数，返回同一 `StatusBuckets` 类型；**零副作用**（不碰 clock/attempts/token）。
- 三处实现缺一不可（trait 方法加在 trait 上，`pg.rs`/`fake.rs` 编译期强制）；truth-check 不允许零调用——调用方 = R2 sampler + R4 测试。

### R2 — 运行期 gauge sampler（aero-server，只读）

- `crates/aero-server/src/metrics.rs` 新增（`sample_pg_health` :202 同款形态）：
  - const：`AUDIT_OUTBOX_STATUS: &str = "aero_audit_outbox_status"`（labeled gauge，label `status` ∈ `{enqueued, claimed, delivered, dead}`——AI_DLQ `kind` label 先例 :119/:152-185）与 `AUDIT_PROVISION_FAIL_CLOSED: &str = "aero_audit_provision_fail_closed"`（0/1 indicator，R3）。
  - `pub async fn sample_audit_outbox(pool: &sqlx::PgPool) -> AuditOutboxSample`，返回 `{ enqueued, claimed, delivered, dead }`（+ relay 谓词，见 R3）；查询**全部只读**；单查询 Err → `tracing::warn!` + **保留旧 gauge 值**（"query Err 留旧值"，与 AI DLQ block :183 一致）——**永不 panic、永不改状态**。
- `crates/aero-server/src/bin/boot/metrics_tasks.rs` 新增 timer block（AI DLQ block :152-185 同款）：30s、`MissedTickBehavior::Skip`、共享 cancel token（`ai_shutdown`）；每 tick 用 `PgOutboxRepo::new(state.pg.clone())` 调 `sample_audit_outbox` → `set_gauge_labeled(AUDIT_OUTBOX_STATUS, n, &[("status", …)])` × 4；`register_help` 在 block 内（index-size block 先例）。
- **运行条件**：**不依赖 `AERO_AUDIT_TOKEN_ENDPOINT`**——relay 缺席时 gauge 照样采样（「dead 行累积不可见」的修复恰恰要求 relay 缺席也有观察面）；表缺席 → warn + 留旧值（F13 降级）。

### R3 — 配给门自动反馈（health signal → verdict）

- 每 tick（与 R2 同循环或紧邻）：采样快照之外再读 **Q0 relay 谓词**（`SELECT runtime.enabled, (SELECT count(*) FROM snaplink_commercial_bindings WHERE enabled) FROM snaplink_commercial_runtime runtime WHERE runtime.singleton`——audit_provision.rs Q0_SQL :34 同款，落 `sample_audit_outbox` 或同模块小函数）。
- 组装 `aero_eng::audit_provision::AuditSnapshot { relay_enabled, enabled_bindings, v1: V1OutboxCounts::default()/*见 D3*/, g0239: Some(G0239Counts{ table: Some("audit_governance_outbox"), pending/claimed/delivered/dead 来自 buckets, oldest_pending_secs: None, dead_rows: vec![] }), priority_landed: false, class_landed: false }` → `aero_eng::audit_provision::verdict(&snapshot)`（aero-server Cargo.toml :30 已有 aero-eng，零新依赖）。
- **FailClosed 时自动 surface**（不再等 CLI）：`tracing::error!` 打一行 greppable `audit-provision-check: verdict: fail-closed — <verdict 原文>` + `set_gauge(AUDIT_PROVISION_FAIL_CLOSED, 1.0)`；非 FailClosed → `set_gauge(…, 0.0)`（Consistent/Healthy 都算 0）。
- **dead 臂无条件触发**：`verdict()` dead 优先（E1），403 → `mark_dead`（E3）后下一次 tick（≤30s）即 FailClosed——「403 → dead 行 → 门自动 FailClosed」成立，无需任何人跑 CLI。
- **只读**：全程 SELECT + 日志 + gauge；不 UPDATE `snaplink_commercial_runtime`（§7 D2 否决开关翻转）、不动任何 outbox 行。

### R4 — state_machine.rs 测试扩展（403 → 门 FailClosed 全链）

`crates/aero-audit-connector/tests/state_machine.rs` 新测试（命名如 `forbidden_dead_surfaces_as_provisioning_gate_fail_closed`），复用 A1-4 `forbidden_dead_on_first_attempt`（:242-266）的 stub/fake 装置：

1. `StubSink` behavior `events_status: 403` + `FakeOutbox` seed 1 行（pin 时钟）；
2. `relay.dispatch_batch()` → 断言 `fake.row(id).status == FakeStatus::Dead`、`attempts == 1`、`stub.posts() == 1`（既有 A1-4 断言保留）；
3. **新增**：`fake.status_buckets()`（R1）→ 断言 `dead == 1` 且 `enqueued/claimed/delivered == 0`；
4. **新增（门反馈）**：用 buckets 组装 `aero_eng::audit_provision::AuditSnapshot`（relay_enabled=true、enabled_bindings=1、v1 全零、g0239.dead=1）→ `verdict(&snapshot)` → 断言 `matches!(Verdict::FailClosed(reason))` 且 reason 含 `"dead"`；
5. **新增（只读钉）**：`status_buckets()` 前后 row 快照（status/attempts/available_at/claim_token）逐字段相等——采样不 mutation。

依赖：connector `Cargo.toml` `[dev-dependencies]` 加 `aero-eng.workspace = true`（§1.1：aero-eng 无 sqlx、无 connector 依赖，不成环；dev-dep 不污染生产依赖树）。

### R5 — 回归保持（不变量）

- **T-11 drill**：`aero-audit-t11-drill` 的 `COUNT(status=0)==N` / `COUNT(status IN (1,2,3))==0` / `SUM(attempts)` 逐轮断言不变——sampler/R1 若 mutation 即打爆（天然守卫）。
- **priority drill**：`moderation-in-first-batch` / `drain-501` / `parity-501` 不变。
- 既有 `state_machine.rs` 8 测试 + `relay.rs` 单测 + `pg.rs` db_tests 全绿；`crates/aero-eng/tests/audit_provision.rs` 22/22 绿（verdict 函数零改动，只新增调用方）。
- 提交门禁：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（不新增警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规。

## 5. Acceptance checks（direction 原句保留，逐条 testable）

> 原句：*"Boot timer samples `audit_governance_outbox` status buckets into Prometheus gauges (pending/claimed/delivered/dead) with no state mutation; a relay-health signal (e.g., dead-count or last-403 timestamp) consumed by the provisioning gate so a 403 → dead row flips verdict to FailClosed automatically, not only when the CLI runs; extend crates/aero-audit-connector/tests/state_machine.rs with a test asserting mark_dead on 403 surfaces as a provisioning-gate fail-closed condition; T-11 and aero-audit-priority-drill stay green."*

| AC（原句片段） | 可测断言（测试形式） | 位置 |
|---|---|---|
| **AC1** Boot timer samples `audit_governance_outbox` status buckets into Prometheus gauges (pending/claimed/delivered/dead) with no state mutation | **Oracle A（单元，无 DB）**：`FakeOutbox::status_buckets()` 对 4 状态行集（seed 各状态）返回精确 `StatusBuckets`；再调 `claim_due`/`settle`/`mark_dead` 后 bucket 同步翻转（采样面与状态机一致）。**Oracle B（PG-gated）**：`pg.rs` db_test（`#[ignore]` + `DATABASE_URL`）——`ensure_outbox_table` + seed 已知分布 → `PgOutboxRepo::status_buckets()` 精确匹配 + **行集逐行不变**（`SELECT` 前后 event_id/status/attempts/claim_token 全等）。**Oracle C（sampler 函数）**：`aero_server::metrics::sample_audit_outbox` 对 PG-gated 种子库返回 buckets 并在 `aero_audit_outbox_status{status=…}` gauge 上可见（`/metrics` bearer 门控冒烟，AGENTS §4.3 前台/pkill）。**Oracle D（无 mutation 运行态）**：T-11 DB 上起 server（或直跑 sampler 函数）→ `COUNT(*)`/`SUM(attempts)` 前后相等（drill 断言同源）。 | R1/R2 + `crates/aero-audit-connector/src/pg.rs` tests + `crates/aero-server/src/metrics.rs` |
| **AC2** relay-health signal (dead-count) consumed by the provisioning gate so a 403 → dead row flips verdict to FailClosed automatically, not only when the CLI runs | **Oracle A（全链单测）**：R4 测试——403 → Dead → buckets.dead=1 → `verdict()` → `FailClosed`（reason 含 "dead"）。**Oracle B（运行态自动 flip）**：throwaway 已迁移库 seed 1 行 status=3（`last_error='audit sink rejected the service identity (HTTP 403)'`）→ 起 server（`AERO_AUDIT_TOKEN_ENDPOINT` 配 loopback 或**不配**——sampler 独立于 relay presence）→ 一个 tick 内（≤30s，短命跑完 `pkill aero-server`）日志出现 `audit-provision-check: verdict: fail-closed`（ERROR）且 `aero_audit_provision_fail_closed` = 1；`DELETE` dead 行 → 下一 tick 日志回归非 fail-closed 且 gauge = 0。**Oracle C（CLI 不受影响）**：`cargo run -p aero-cli -- audit-provision-check` 对同库仍输出同一 verdict（手动 oracle 与运行态同源同义）。 | R3 + R4 + harness 冒烟段 |
| **AC3** extend crates/aero-audit-connector/tests/state_machine.rs with a test asserting mark_dead on 403 surfaces as a provisioning-gate fail-closed condition | **Oracle**：`cargo test -p aero-audit-connector --test state_machine` → 新测试绿（R4 五步断言：403 → Dead → buckets → `verdict()` FailClosed → 只读钉）；既有 `forbidden_dead_on_first_attempt` 保持绿（回归）。 | R4（`tests/state_machine.rs`） |
| **AC4** T-11 and aero-audit-priority-drill stay green | **Oracle A**：`bash scripts/test-integration.sh`（PG 可用）→ `B5-CHECK t11-fail-closed: PASS` 与 `B5-CHECK moderation-priority-drill: PASS`（`scripts/b5-pin.sh` 37-slot 内，legs 已在 :446-461 区）。**Oracle B**：`cargo run -p aero-audit-connector --bin aero-audit-t11-drill`（throwaway 库）exit 0；`--bin aero-audit-priority-drill` exit 0（moderation-in-first-batch / drain-501 / parity-501 PASS 行）。**Oracle C**：`cargo test --workspace --lib` 全绿 + connector 全测试绿。 | R5（零改动保持） |

## 6. Test placement

| Test | Location | Harness |
|---|---|---|
| `StatusBuckets` fake 精确性 + 状态机同步（AC1-A） | `crates/aero-audit-connector/src/fake.rs` 或 `tests/state_machine.rs` | `cargo test -p aero-audit-connector`（无 DB） |
| `status_buckets` PG 实现 + 只读钉（AC1-B） | `crates/aero-audit-connector/src/pg.rs` tests（`#[ignore]` + `DATABASE_URL`，`ensure_outbox_table` 先例） | `cargo test -p aero-audit-connector -- --ignored`（throwaway 已迁移库） |
| 403 → 门 FailClosed 全链（AC2-A/AC3） | `crates/aero-audit-connector/tests/state_machine.rs` 新测试 | `cargo test -p aero-audit-connector --test state_machine`（无 DB） |
| `sample_audit_outbox` + gauge 写面（AC1-C） | `crates/aero-server/src/metrics.rs`（`sample_pg_health` 先例）+ `metrics_tasks.rs` timer block | `cargo test -p aero-server --lib`（PG-gated 处 `--ignored`）+ `/metrics` 冒烟 |
| 运行态自动 flip（AC2-B） | `scripts/test-integration.sh` 新冒烟段（复用 `AUDIT_PROVISION_DB` throwaway 流程 + 前台/setsid + pkill 纪律，AGENTS §4.3） | `bash scripts/test-integration.sh` |
| T-11 / priority drill 回归（AC4） | `crates/aero-audit-connector/src/bin/aero-audit-t11-drill.rs` / `aero-audit-priority-drill.rs`（**零改动**） | harness legs（`b5_check` 槽）+ 直跑 exit 0 |
| verdict 语义 oracle（不变） | `crates/aero-eng/tests/audit_provision.rs`（22 项，**零改动**） | `cargo test -p aero-eng --test audit_provision` |

## 7. Risks / 决策点 / 红线

- **D1 — health signal 二选一钉为 dead-count**：direction 给「dead-count or last-403 timestamp」，acceptance 的 flip 条件是「403 → dead 行 → FailClosed」——verdict 规则就是 `dead > 0`（E1），**dead-count 是唯一必需信号**；last-403 时间戳（`max(available_at)` of status=3）是可选衍生 gauge，不做也不违反 acceptance（dead 行的 `last_error` 已带 403 文案，Q5 明细 CLI 可查）。**不做**。
- **D2 — 「automatic runtime-switch flip」否决**：direction proposed #1「flip `snaplink_commercial_runtime.enabled` on dead accumulation」**不实施**——该开关是 0235 商业 enforcement 总闸（Q0 事实源），自动翻转会连带禁用 v1 outbox/usage 等无关 enforcement，且 acceptance 的 flip 宾语是 **verdict**（"flips verdict to FailClosed"）。fail-closed 的自动面 = ERROR 日志 + fail-closed gauge + 门 verdict 运行期评估（R3）。
- **D3 — 运行期快照的 v1 桶置零**：运行期 `AuditSnapshot.v1` 用全零（`V1OutboxCounts::default()`）——v1 `snaplink_delivery_outbox`（0235 遗留面）是 CLI/boot 门的评估面，本 direction 的运行期循环只管 0239 governance 桶 + dead 臂；`verdict()` 的 dead 臂（dead 优先）不受 v1 影响，正确性成立。CLI 仍是 v1 臂的 oracle。
- **D4 — 表缺席降级**：pre-0239 boot（F13 同款）→ `status_buckets` Err → warn + 留旧值；sampler 永不 crash、永不 panic（AGENTS §2「query Err 留旧值」）。
- **D5 — 30s tick 延迟**：403 → 自动 FailClosed 的可见延迟 ≤ 一个 tick（30s，AI DLQ/NATS backlog 同档）；这是 ops 信号不是投递门，不背压、不进事件路径。
- **最高风险：batch 产物 untracked**（与 sibling specs 同判）：governance.rs、audit_provision.rs(+tests)、aero-audit-connector/、migrations 0239-0241 等全未提交——`git reset --hard master`（AGENTS §4.1）会抹掉本 direction 全部验收对象；提交是 AC1-AC4 前提。
- **与 sibling boot-gate spec 的边界**：`2026-08-08-aero-ai-b5-4-relay-boot-provisioning-gate.req.md`（未落地）规划 boot 期一次性 fail-loud 预检；本 direction 是**运行期**循环（≤30s 持续评估 + gauge + 日志）。两者互补：boot 门 = 启动拒绝，本 direction = 运行期自动翻转与可观测。若 sibling 后续落地，`sample_audit_outbox` 的快照组装可被 boot 门复用（同函数，不重复 SQL）。
- **aero-server 改动面**：`metrics.rs`（新 const + 新 sampler 函数）+ `metrics_tasks.rs`（新 timer block）——不碰 main.rs、不碰 health.rs/readyz、不碰 relay spawn。
- **connector 改动面**：`outbox.rs`（trait + StatusBuckets 类型）、`pg.rs`、`fake.rs`（三处实现）+ `Cargo.toml` dev-dep + `tests/state_machine.rs` 新测试——**relay.rs/client.rs/config.rs 零改动**。
- **truth-check**：新函数必须有调用（R2 sampler 调 `status_buckets`/`sample_audit_outbox`，R4 测试调 `verdict`）——不加 allowlist 条目。

## 8. Sequencing

1. **提交 batch 产物**（前置，§7 最高风险）：按 batch 提交规范归组提交 untracked B5 文件。
2. **基线确认**：`cargo test -p aero-eng --test audit_provision`（22/22）→ `cargo test -p aero-audit-connector`（既有全绿）→ `bash scripts/test-integration.sh` T-11/priority legs 绿。
3. **R1 落地**：`StatusBuckets` + trait 方法 + `pg.rs`/`fake.rs` 实现 + PG-gated db_test（AC1-A/B）。
4. **R4 落地**（先行——connector 内闭环，无服务依赖）：dev-dep `aero-eng` + state_machine.rs 新测试（AC2-A/AC3）。
5. **R2+R3 落地**：`aero-server/src/metrics.rs` 新 const + `sample_audit_outbox`（+Q0 谓词）→ `metrics_tasks.rs` timer block（register_help + set_gauge_labeled + verdict 评估 + ERROR 日志 + fail-closed gauge）。
6. **运行态冒烟**（AC1-C/AC2-B）：throwaway 库 seed dead 行 → 起 server → 一个 tick 内日志 `audit-provision-check: verdict: fail-closed` + gauge=1；`/metrics` 可见四桶；行集不变。
7. **全链门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 · `bash scripts/test-integration.sh`（T-11/priority 槽 PASS，AC4）。
