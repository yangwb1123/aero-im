# B5-4（connector slice）— fail-closed 运行化设计：dead/pending 采样成 gauge + 配给门自动反馈（非手动 CLI）

> Module: `crates/aero-audit-connector`（只读采样面）+ `crates/aero-server`（sampler timer + 门反馈）。
> Requirement record: `docs/requirements/2026-08-08-aero-audit-connector-b5-4-fail-closed-operational.req.md`（R1–R5，AC1–AC4）。
> Seam 契约锚点（已落地）：`docs/design/2026-08-07-aero-cli-b5-4-audit-provision-check-psql.design.md`（CLI psql 检查，`aero_eng::audit_provision`）+ `docs/requirements/2026-08-07-aero-cli-b5-4-audit-provision-check.req.md`。
> Sibling（勿撞）：`docs/requirements/2026-08-08-aero-ai-b5-4-relay-boot-provisioning-gate.req.md`（**requirements 态未落地**——`decide_relay_boot` 全仓零命中，见 §0.6；本设计与其互补：boot 门 = 启动拒绝，本设计 = 运行期自动 flip + 可观测）。
> Status: **proposed design — REV 2**（2026-08-08）。REV 2 采纳三轮评审（sql_perf_reviewer / observability_reviewer / audit_integrity_security_reviewer）的修正，**重钉 AC1–AC4 oracle** 覆盖 F1/F2/F3、三值 indicator、边沿 ERROR、两档 SQL 探针——采纳表见 §0.3，oracle 重钉见 §6。§0 证据核对基于当前工作树，行号为核对时锚点、可能漂移——**文件/符号**才是稳定 grep 锚点。
>
> **Current B5 pin (2026-08-26)**：live `B5_CONTRACT_TEST_LIST` 为 48 个 slot（27 个非 `[PROPOSED]` 可执行 slot + 21 个 `[PROPOSED]` 仓外占位）。历史 REV2 草案中的 37→38 计划不代表当前清单；“executed”是 manifest 分类，不是本设计声称已经运行的测试数。

## 0. Evidence verification verdict（5 条证据逐条复验）

| # | Cited evidence | 复验结果（本工作树） |
|---|---|---|
| E1 | `crates/aero-eng/src/audit_provision.rs` — verdict 矩阵（dead 优先）+ CLI-only | ✅ **Verified**。`verdict()` 三态矩阵 dead 优先：`g0239.dead > 0` → `FailClosed("…dead is never counted delivered")`；`!(enabled && bindings>0)` ∧ undelivered>0 → `FailClosed`；否则 `Consistent`/`Healthy`。**CLI-only 成立**：唯一执行入口是 `crates/aero-cli/src/main.rs` 的 `AuditProvisionCheck_` 臂（`audit-provision-check [--priority]` → `run`/`run_priority`）；`aero-eng/Cargo.toml` 无 sqlx、无 connector 依赖（aero-common/tokio/serde_json/anyhow/async-trait/time/serde/toml），psql 走子进程。22 单测在 `crates/aero-eng/tests/audit_provision.rs`。 |
| E2 | `crates/aero-server/src/bin/boot/metrics_tasks.rs` — sampler 清单无 audit gauge | ✅ **Verified**。`spawn_all` 内 sampler：DB pool+WHIP 15s、index-size 60s、PG stats 60s、AI DLQ 30s、NATS backlog 30s、subscriber lease 30s、call/stream 心跳 30s、SFU REMB tick 1s——**无 audit 面**。`crates/aero-common/src/metrics.rs` names 无 audit 名。sampler 模式：`MissedTickBehavior::Skip` + 共享 cancel token + query Err → `tracing::warn!` 留旧值（永不改状态）。 |
| E3 | `crates/aero-audit-connector/src/relay.rs` — `DeliveryError::Forbidden` → `mark_dead` 立即死，无消费者 | ✅ **Verified**。`deliver_claim`：403 → `mark_dead(event_id, token, attempts, "audit sink rejected the service identity (HTTP 403)")`，注释明示「Deliberately NOT `is_dead_at`: HTTP 403 is fail-closed immediate」；fence 丢失仅 `warn!`。dead 行除 CLI（psql 读取）外**无任何运行期消费者**——gap 成立。 |
| E4 | `migrations/0239_audit_governance_outbox.sql` — enqueue 门 = runtime.enabled only | ✅ **Verified**。`aero_enqueue_governance_audit()`：Gate 1 = `snaplink_commercial_runtime.enabled`（`COALESCE(enforcement_enabled, FALSE)` 缺行即跳过，fail-open）；Gate 2 = `aero_snaplink_binding_for_workspace`（fail-closed RAISE）；token-keyed 仅 `message.moderated`。**零 relay-health 咨询**。0239 表含 `status CHECK (0..3)` + `last_error` + `available_at`/`created_at`——dead 行时间戳/文案可派生，无需新列。 |
| E5 | `crates/aero-server/src/bin/main.rs:251-266` — relay spawn presence-gated，无 health hook | ✅ **Verified（行号精确）**。`RelayConfig::from_env()` :251 → `Ok(Some)` 分支 :252-262（PgOutboxRepo + AuditClient + `tracker.spawn(relay.spawn(...))` :260）；`Ok(None)` 跳过；`Err` fail-loud。`spawn_metrics_tasks` :270。**main.rs 全文件无 `audit_provision|provisioning` 命中**——无 verdict 咨询。presence gate 本体在 `config.rs:49-63`（`AERO_AUDIT_TOKEN_ENDPOINT` 缺席且无其他 `AERO_AUDIT_*` → `Ok(None)`）。 |

### 0.1 补充核对（设计依赖面，REV 2 增补）

| 检查项 | 结果 |
|---|---|
| `OutboxRepo` 实现者数量 | **恰 2 个**：`pg.rs:80`（`PgOutboxRepo`）+ `fake.rs:191`（`FakeOutbox`）——trait 加方法只需补这两处，编译期强制（无第三实现者漏改） |
| `OutboxStatus::from_i32` | `aero-common/src/model/audit.rs:20-50`：`const fn`，未知 → `None`（fail-open 解析，与 Q3 桶解析同语义）；`as_i32` const 可用 |
| `V1OutboxCounts` 是否实现 `Default` | **否**（`audit_provision.rs:72` 仅 `derive(Debug, Clone, PartialEq, Eq)`）——req R3 的 `V1OutboxCounts::default()` **编译不过**；本设计 §2.4 沿用显式字面量构造，**aero-eng 零改动**（见 D1） |
| `AuditSnapshot` / `G0239Counts` 字段可见性 | 全部 `pub` 字段（`audit_provision.rs:72-110`）——sampler 可跨 crate 字面量构造 |
| aero-server 依赖 | `Cargo.toml:29` aero-common、`:30` aero-eng、`:36` aero-audit-connector——**sampler 零新生产依赖** |
| connector → aero-eng dev-dep 环 | aero-eng 无 sqlx、无 connector 依赖（§0 E1）——`[dev-dependencies] aero-eng.workspace = true` **不成环** |
| `register_help` 幂等性 | `metrics.rs:458-467`：「Idempotent for matching kinds」——timer block 内注册安全 |
| `set_gauge` / `set_gauge_labeled` / **`inc_counter`** | `metrics.rs:881/886` 全局便捷函数 + **`common_metrics::global().inc_counter(name, delta)`（:491，`MetricKind::Counter`）**——sample-error counter 有现成 API |
| 既有 403 测试 | `tests/state_machine.rs:242-266` `forbidden_dead_on_first_attempt`（403 → Dead、attempts==1、posts==1、不再可 claim）——R4 新测试在其上叠加，**不动其断言** |
| tokio 首 tick 语义 | `Cargo.lock` tokio **1.52.3**：`interval()` = `interval_at(now, …)`——**首 tick 立即触发**；heartbeat 块（metrics_tasks.rs :242/:348/:411）pre-loop `tick().await` 丢弃首 tick，sampler 块（AI DLQ :142、NATS :174、pg-health :116）**不丢弃**——新块照 sampler 模式（observability (c)） |
| `AERO__SERVER__*` env 先例 | `retention.rs:144` `std::env::var("AERO__SERVER__RETENTION_SWEEP_SECS")`（figment 双下划线，0 禁）——慢节奏 env 同款 |
| `/metrics` bearer 门控 | `metrics.rs:291-323`：`AERO_METRICS_TOKEN` 非空即要求 bearer——冒烟 curl 需带 token |
| `b5-pin.sh` 槽位守卫 | `scripts/b5-pin.sh` 当前 `B5_CONTRACT_TEST_LIST` 恰 **48** 槽（27 个可执行 slot + 21 个 [PROPOSED]）；`assert_b5_contract_pin` 以 48 为硬钉。历史 REV2 计划中的 37→38 已被后续清单演进取代。 |
| harness CLI 侧 F3 钉已存在 | `test-integration.sh` leg B2（约 :270-296）：switch off + 1 条 v1 undelivered audit 行 → CLI exit≠0 + `verdict: fail-closed` + `no audit:event:write grant issued`——**CLI 侧 v1 臂分歧已钉**，sampler 侧钉在 §2.7 C2（F3 采纳） |

### 0.2 direction 前提钉化（复验结论）

- **「fail-closed 仅靠手动 CLI 强制」成立**：`rg decide_relay_boot` 全仓仅命中 sibling req 文档自身（`docs/requirements/2026-08-08-aero-ai-b5-4-relay-boot-provisioning-gate.req.md`），main.rs relay 分支零 verdict 咨询（E5）。本设计不与之重叠：boot 门（若 sibling 后续落地）= 启动期一次性预检；本设计 = **运行期 ≤30s 持续评估 + gauge + ERROR 日志**，可独立落地。**indicator 目前零消费者（ops-signal only，直到 sibling boot 门落地）——§1/§4 显式声明，不宣称投递门语义**（audit_integrity (d)）。
- **「dead 行累积不可见」成立**：metrics_tasks.rs sampler 清单（E2）无 audit 面；`aero-common::metrics::names` 无 audit 名。
- **「enqueue 门不感知 relay 死活」成立**（E4），但**不在本设计范围**：改触发器 = 新迁移 + 生产者侧语义（A2 half(4) 已钉契约 `moderation_finalize_runtime_disabled_commits_1_plus_0`），§3 红线。
- **依赖可行性成立**（§0.1 表）。

### 0.3 REV 2 评审采纳表（三轮评审 → 修正落点 → oracle 钉）

> 评审产物：`docs/auto/runs/make-b5-4-fail-closed-operational-relay-dead-pen-9d0b77ba/artifacts/adversarial_review-9c87f3a7/metrics-alerting-semantics-review.md`（observability）、`…/meta/audit_integrity_security_reviewer.md`（integrity）、sql_perf_reviewer 证据（perf，结论见 §0.3 注）。每条 finding 的修正落点 = 设计章节；oracle 钉 = §6 表行。**全部修正 aero-server/connector-local；`verdict()`、migrations、relay 状态机零改动**。

| # | Finding（评审） | 修正落点 | Oracle 钉（§6） |
|---|---|---|---|
| **F1**（integrity, HIGH） | 持久 constituent-query 失败（如 0235 丢、0239 在）→ verdict 永不评估 + relay 继续 mark_dead → indicator 冻结 0。warn+留旧值是 observability 姿态，不是 fail-closed 门姿态 | §2.5 失败 tick → indicator=**2（unverifiable）** + sampler_up=0 + errors counter + 边沿 ERROR——绝不冻结 0（三值 indicator，D7） | AC2-B leg L4（DROP TABLE → 2）；AC1-C |
| **F2**（integrity, HIGH） | §2.5 草图 gauges/verdict 无条件写 → FM1 首 tick 失败发布 **fabricated 全零快照**，pre-0239 库看起来 healthy-empty 而非 unverifiable | §2.4 `sample_audit_outbox() -> Result<…>`（任一查询 Err → 整样本 Err，**不组装部分/全零快照**）；§2.5 Err 分支不写桶 gauge | AC2-B leg L4 断言 indicator **≠ 0**；AC1-C |
| **F3**（integrity, HIGH for (d)） | D3 v1-zeroing 是**可达的、未文档化的**同库分歧：0236 v1 触发器对**所有** action（含 `message.moderated`，无 token-keyed 排除）enqueue `destination='audit'`；switch off + v1 undelivered → CLI=FailClosed、sampler=Consistent(0) | §4 FM3′ 文档化 + §5 改「同库同义」声明 + D3 修订；**分歧本身不改**（v1 臂属 CLI/boot 门；运行期循环 0239-only 是 D3 既定） | AC2-C C2（connector 单测钉 sampler 侧 Consistent）+ 既有 harness leg B2（CLI 侧 FailClosed 已钉）——**两 oracle 各自钉住，分歧显式化** |
| 三值 0/1/2 indicator（integrity） | 「evaluated: not fail-closed」与「not evaluated」必须可分 | §2.4 `indicator_value` + §2.5 Option 状态；gauge 名 `aero_audit_provision_indicator`（0/1/2） | metrics.rs 单测 + AC2-B L4 |
| 边沿 ERROR（observability (e)1） | 草图每 tick ERROR（2880/天）：`fresh` 算了没用、match 无条件跑；等值门还混淆「查询失败留旧值」与「数据真没变」 | §2.5 ERROR **0→1 边沿**（+ 进 unverifiable 边沿），gauge **level-triggered**；`verdict_error_edge` 纯函数 | metrics.rs 单测（12 组合）+ AC2-B 冒烟 `grep -c` 恰 1 |
| `fresh` 删除（integrity F6 / observability (e)1） | `fresh = sample != last` 是**第二个编译 break**（`AuditOutboxSample` 无 `PartialEq`）；修了也永不触发 → indicator 系列永不写 | `fresh` **删除**；`Option<AuditOutboxSample>` 失败语义（Result）；新类型全部 derive `Default + PartialEq`（D6） | §6 钉文件核对表 |
| 两档 SQL 探针（perf） | 全量 `GROUP BY` 是 O(累计表大小)，0239 无 sweeper（status 2/3 永久累积）→ 30s 每实例全表扫描 + 缓存污染 | §2.1/2.2 **Tier-1（30s）**：partial-index EXISTS 路径（`status IN (0,1)` 计数 + `EXISTS status=3`）+ Q0；**Tier-2（慢节奏）**：全量聚合 `AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS`（默认 300，0 禁） | AC1-A/B（probe + buckets 双面）+ AC1-C /metrics + AC2-B |
| F6/`≤30s` 精度（observability (c)(d)） | 「首 tick = 无系列」是错误 gap 非时序 gap（首 tick 立即触发）；≤30s 缺 phase/query/relay-poll 精度 | §2.5 无 pre-loop discard（sampler 模式非 heartbeat 模式）；FM4 重述 bound | AC2-B 时序断言 |
| max() 聚合 + 缺席告警（observability (b)） | 多实例必须 `max()`（fail-safe stale-high）；FM1 上 `absent()` 永久误报 | §4 FM7 写 max() 契约；**sampler_up gauge**（1=任一本轮成功）取代 absent()（D10） | AC1-C /metrics `sampler_up` |
| dead-bucket ⟺ dead-arm（observability (a)6） | `{status="dead"}>0` ⟺ verdict dead 臂；indicator 边际价值 = disabled-with-backlog 臂 | §4 FM4/§8 D5 文档化等价 | AC2-B（两信号同 tick 断言） |
| counter 否决（observability (a)） | transitions counter 多实例 N 倍重复 + restart 重置事件史；拒绝 | §8 D5 显式拒绝；**sample-errors counter 保留**（per-instance 本地错误计数，`sum()` 语义正确——与 transitions counter 不同类） | AC1-C `errors_total` |
| self-hiding 盲区（observability (e)2） | enabled + bindings=0 + 空 outbox → RAISE 阻止 enqueue → verdict Consistent——真实阻断态自隐 | §8 D5 文档化覆盖极限（boot 门才是阻断探测器） | 文档声明（无 oracle——行为正确） |
| F11（integrity bonus） | transient-forever blackhole / relay-absent-with-switch-on 逃逸；`oldest_pending_secs` 硬编码 None | §2.4 Q4-mirror `oldest_pending_secs` 进慢节奏样本（D11） | AC1-C /metrics `aero_audit_outbox_oldest_pending_secs` |
| first-tick 立即（observability (c)） | 别「对齐」heartbeat 的 pre-loop discard（那会制造 30s gap） | §2.5 显式声明 | AC2-B（秒级出现断言） |
| 结构化字段 + greppable 前缀（observability (e)3） | 静态消息 + 字段两全（background.rs/sampler 先例）；harness grep CLI 报告非 server 日志 | §2.5 ERROR 行形态 | AC2-B grep 前缀 |
| SQL 成本诚实性（perf caveat） | dead=0 时 dead 存在性探针 = 一次 bounded heap pass（零迁移不可消除）；根因 = 缺 sweeper（越界） | §2.2 注释 + §8 风险；**不在本设计补 sweeper**（写路径迁移，越界） | 文档声明 |

> perf 评审核心结论（已复验，采纳为 §2 SQL 依据）：① 0239 无 `status` 独立索引、status 2/3 无任何索引，全量聚合无 index-only 路径 = O(表大小)；② 无 sweeper GC 此表（retention.rs 17+ sweeps 零 outbox 表；`sweep_audit` 只清源表 audit_events），status 2/3 终态永久累积；③ `verdict()` 只消费 `> 0` 谓词——EXISTS 探针语义字节等价；④ Q0 本身 fine（singleton 1 行 PK + bindings 每 workspace 一行）。

## 1. Design overview

把「fail-closed 只靠手动 CLI」改成「运行期自动」：在既有的 30s sampler 框架（AI DLQ / NATS backlog 同款）里加一个只读 audit 面——

1. **connector 长出只读采样面（两档）**：`OutboxRepo` trait 加 `verdict_probe()`（**Tier-1，30s 判定路径**：partial-index 计数 + EXISTS 存在性，成本 ∝ due 集，与累计表大小无关）与 `status_buckets()`（**Tier-2，慢节奏全量聚合**：默认 300s，四桶精确值）。`FakeOutbox` 从内存行集实现同一语义。**纯 SELECT，无 FOR UPDATE / UPDATE / DELETE**（AGENTS §2「永不改状态」）。
2. **aero-server 跑 30s gauge sampler**：`metrics.rs` 加 `sample_audit_outbox(&pool) -> Result<…>`（任一查询失败 → **Err**，绝不发布 fabricated 全零/部分快照——F2 修复）与慢节奏 `sample_audit_outbox_full(&pool)`；`metrics_tasks.rs` 加两个 timer block，把探针/桶写进 `aero_audit_outbox_status{status=…}`。
3. **配给门自动反馈（三值 indicator + 边沿 ERROR）**：同一 tick 用探针快照 + Q0 谓词组装 `AuditSnapshot` → 真 `verdict()` → `aero_audit_provision_indicator`（0=not-fail-closed / 1=FailClosed / **2=unverifiable**）+ `aero_audit_provision_sampler_up`（0/1）+ `aero_audit_outbox_sample_errors_total`（counter）。**ERROR 只在边沿打**：进 FailClosed（0→1）与进 unverifiable（{0,1}→2）各一次，持续态只由 gauge 表达（level-triggered）。于是 **403 → mark_dead（E3）→ 下一 tick（≤30s）门自动 FailClosed**，不依赖任何人跑 CLI；查询失败 → 显式 unverifiable，**不再冻结在 0**（F1 修复）。
4. **全链测试**：`tests/state_machine.rs` 新测试把 403 → Dead → 探针 → buckets → verdict → FailClosed 六步闭环（含只读钉 + 探针/精确双源 parity）；`metrics.rs` 纯函数单测（indicator 映射 + 边沿矩阵 + 快照组装三分支）；`pg.rs` `#[ignore]` db_test 钉 PG 侧两档探针；harness 新冒烟段（§2.10）钉运行态 flip / unverifiable / 边沿 ERROR 计数 / /metrics 可见性。
5. **零回归**：T-11 / priority drill 的 COUNT/SUM 断言天然是 no-mutation 守卫——sampler 若敢写一行，drill 即红。

**不做什么**（红线）：不翻转 `snaplink_commercial_runtime.enabled`（0235 商业 enforcement 总闸，翻转会连带禁用 v1 outbox/usage 等无关 enforcement；acceptance 的 flip 宾语是 **verdict**）；不改 0239 enqueue 触发器；不改 relay.rs/client.rs/config.rs 状态机；不改 CLI/readyz/aero-common；零新迁移；**不补 outbox 终态行 sweeper**（写路径迁移，perf 评审越界项——indicator 语义不依赖表大小，见 §8）。**indicator 是 ops-signal**（无消费者直到 sibling boot 门落地）：本设计是「运行期持续评估 + 可观测」，boot 门才是投递阻断。

## 2. API changes（全部签名与落点）

### 2.1 connector — `crates/aero-audit-connector/src/outbox.rs`（R1）

新增（trait 上，紧邻既有五方法之后；类型放 trait 同文件）：

```rust
/// Tier-1 判定路径探针（30s tick 用）：0239 状态 0/1 的**精确计数**
/// （partial-index served，成本 ∝ due 集，与累计表大小无关）+ status=3
/// **存在性**（EXISTS，heap early-exit）。`verdict()` 只消费 `> 0` 谓词，
/// 因此该探针与 CLI 的 Q3 精确桶**判定语义字节等价**（perf 评审 §4）。
/// 字段顺序与 `aero_common::model::audit::OutboxStatus` 一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VerdictProbe {
    /// status=0 精确计数（同一查询带出，兼作 enqueued gauge 数据）。
    pub enqueued: i64,
    /// status=1 精确计数（兼作 claimed gauge 数据）。
    pub claimed: i64,
    /// status=3 存在性（无 status=3 索引——零迁移下不可消除的
    /// bounded heap pass，见 §2.2 注释）。
    pub has_dead: bool,
}

/// Tier-2 慢节奏全量聚合产物（`AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS`，
/// 默认 300s）：四状态**精确**桶。0239 四状态桶（0=enqueued 1=claimed
/// 2=delivered 3=dead）。只读采样面产物：供 gauge sampler 与门反馈使用，
/// 绝不经此路径写库。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatusBuckets {
    pub enqueued: i64,
    pub claimed: i64,
    pub delivered: i64,
    pub dead: i64,
}

// OutboxRepo trait 内：
/// Tier-1 判定探针：partial-index 计数（status IN (0,1)）+ dead 存在性
/// （EXISTS）。纯 SELECT。0239 表缺席 → `Err(sqlx)`（sampler 侧消化为
/// indicator=2，**不是** warn+留旧值冻结 0——F1/F2）。
async fn verdict_probe(&self) -> Result<VerdictProbe, Error>;

/// Tier-2 全量四桶聚合（镜像 aero-eng Q3_SQL）。只在慢节奏调用——
/// 30s 路径**永不**全表扫描（perf 评审）。纯 SELECT。0239 表缺席 →
/// `Err(sqlx)`（同一消化路径）。
async fn status_buckets(&self) -> Result<StatusBuckets, Error>;
```

- 两类型自带 `Default`（全零）与 `PartialEq`——`::default()` 构造与 `assert_eq!` 断言都是测试钉（D6 编译修复）；这是 connector 内新类型，不触碰 aero-common。
- `Error` 复用既有 `Error::Store(sqlx::Error)`，不加 variant。
- req R1 的 `status_buckets()` 签名**保持原名不变**（Tier-2 语义 = req 原文）；`verdict_probe()` 是评审修正新增的 Tier-1 面（trait 扩展对 2 个实现者是编译期强制，§0.1）。

### 2.2 connector — `crates/aero-audit-connector/src/pg.rs`（R1）

`impl OutboxRepo for PgOutboxRepo` 新增：

```rust
async fn verdict_probe(&self) -> Result<VerdictProbe, Error> {
    // QP1 — partial-index served：due_idx / due_prio_idx 均为
    // `WHERE status IN (0, 1)` 部分索引（0239:55 / 0240:29）——本查询
    // index-served（非 index-only：status 不在索引列，需 heap visibility
    // 校验，但被 due 集界住，成本 ∝ due 集；稳态 ≈ 0）。
    // 一条查询同时给出 enqueued/claimed 精确计数（兼作 gauge 数据）
    // 与 undelivered 存在性（sum > 0）——verdict() 只消费 `> 0`。
    let rows: Vec<(i32, i64)> = sqlx::query_as(
        "SELECT status, count(*)::bigint FROM audit_governance_outbox \
         WHERE status IN (0, 1) GROUP BY status ORDER BY status",
    )
    .fetch_all(&self.pool)
    .await?;
    let mut probe = VerdictProbe::default();
    for (status, count) in rows {
        match OutboxStatus::from_i32(status) {
            Some(OutboxStatus::Enqueued) => probe.enqueued = count,
            Some(OutboxStatus::Claimed) => probe.claimed = count,
            _ => {}
        }
    }
    // QP2 — dead 存在性：status=3 无索引（零迁移约束下不可消除）。
    // dead=0 时 = 一次 bounded heap pass（与全量聚合同页 IO、无
    // per-tuple 聚合），首个含 dead 行的 page early-exit——恰好是信号
    // 有意义的时刻（403 → mark_dead → 下一 tick 一两页内命中）。
    // 根因修复（终态行 sweeper / (status) 索引）越界，见 §8。
    probe.has_dead = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM audit_governance_outbox WHERE status = 3)",
    )
    .fetch_one(&self.pool)
    .await?;
    Ok(probe)
}

async fn status_buckets(&self) -> Result<StatusBuckets, Error> {
    // 镜像 aero-eng audit_provision.rs Q3_SQL（{table} 固定字面量
    // audit_governance_outbox——本 crate 只拥有这张表的语句）。
    // 仅慢节奏调用（默认 300s）：O(累计表大小) 的扫描被摊销 10×；
    // 永远不在 30s 判定路径上（perf 评审）。
    let rows: Vec<(i32, i64)> = sqlx::query_as(
        "SELECT status, count(*)::bigint FROM audit_governance_outbox \
         GROUP BY status ORDER BY status",
    )
    .fetch_all(&self.pool)
    .await?;
    let mut buckets = StatusBuckets::default();
    for (status, count) in rows {
        match OutboxStatus::from_i32(status) {
            Some(OutboxStatus::Enqueued) => buckets.enqueued = count,
            Some(OutboxStatus::Claimed) => buckets.claimed = count,
            Some(OutboxStatus::Delivered) => buckets.delivered = count,
            Some(OutboxStatus::Dead) => buckets.dead = count,
            None => {} // fail-open：未知 status 忽略（0239 CHECK 限 0..3，理论路径）
        }
    }
    Ok(buckets)
}
```

- `count(*)::bigint` 显式 cast，sqlx 直接解 `i64`（CLI 的 psql 文本解析不需要，这里需要类型对齐）。
- 表缺席 → sqlx `UndefinedTable` → `Error::Store`，由 sampler 侧消化为 **indicator=2**（FM1；与 `claim_due` 的 F13 降级同向但**语义升级**：不再假装可评估，见 D4/D7）。

### 2.3 connector — `crates/aero-audit-connector/src/fake.rs`（R1）

`impl OutboxRepo for FakeOutbox` 新增（零副作用，不碰 clock/attempts/token）：

```rust
async fn verdict_probe(&self) -> Result<VerdictProbe, Error> {
    let state = self.state.lock().expect("fake outbox poisoned"); // 既有 Mutex<FakeState>（fake.rs:82-91），内部形态不变
    let mut probe = VerdictProbe::default();
    for row in state.rows.values() {
        match row.status {
            FakeStatus::Ready => probe.enqueued += 1,
            FakeStatus::Claimed => probe.claimed += 1,
            FakeStatus::Dead => probe.has_dead = true,
            FakeStatus::Delivered => {}
        }
    }
    Ok(probe)
}

async fn status_buckets(&self) -> Result<StatusBuckets, Error> {
    let state = self.state.lock().expect("fake outbox poisoned");
    let mut buckets = StatusBuckets::default();
    for row in state.rows.values() {
        match row.status {
            FakeStatus::Ready => buckets.enqueued += 1,
            FakeStatus::Claimed => buckets.claimed += 1,
            FakeStatus::Delivered => buckets.delivered += 1,
            FakeStatus::Dead => buckets.dead += 1,
        }
    }
    Ok(buckets)
}
```

- `FakeStatus`（fake.rs:31-39）四态与 0239 数值一一对应（`code()` :40-46）——与 PG 实现同构。

### 2.4 aero-server — `crates/aero-server/src/metrics.rs`（R2+R3）

新增 const（`INDEX_SIZE_BYTES` :88 同款局部 const，**不动 `aero-common::metrics::names`**）：

```rust
pub const AUDIT_OUTBOX_STATUS: &str = "aero_audit_outbox_status";          // labeled gauge
pub const AUDIT_PROVISION_INDICATOR: &str = "aero_audit_provision_indicator"; // 0/1/2 gauge（REV 2 更名：三值语义下原名 fail_closed 会误导告警作者；未落地，更名零成本）
pub const AUDIT_PROVISION_SAMPLER_UP: &str = "aero_audit_provision_sampler_up"; // 0/1 gauge
pub const AUDIT_OUTBOX_SAMPLE_ERRORS_TOTAL: &str = "aero_audit_outbox_sample_errors_total"; // counter
pub const AUDIT_OUTBOX_OLDEST_PENDING_SECS: &str = "aero_audit_outbox_oldest_pending_secs"; // gauge
```

新增采样类型与函数（`sample_pg_health` :202 同款形态——只读、永不 panic；**差异：返回 `Result`**，F2 修复）：

```rust
/// 一个 30s tick 的判定路径快照：Tier-1 探针 + Q0 relay 谓词。
/// 任一查询 Err → 整个采样 Err（不组装部分/全零快照——F2：绝不发布
/// fabricated 数据）。derive(Default, PartialEq)：测试钉 + 状态变量初值
/// （D6 编译修复；`fresh` 已删除，见 §2.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuditOutboxSample {
    pub enqueued: i64,        // Tier-1 精确（partial-index served）
    pub claimed: i64,         // Tier-1 精确
    pub has_dead: bool,       // Tier-1 EXISTS（dead 存在性）
    pub relay_enabled: bool,  // Q0
    pub enabled_bindings: i64, // Q0
}

/// 慢节奏全量样本：Tier-2 四桶精确 + Q4-mirror 最老 pending 年龄
/// （integrity F11：关闭 transient-forever / relay-absent-with-switch-on
/// 的 zero-progress 盲区；CLI 的 Q4_SQL 同款谓词）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuditOutboxFullSample {
    pub buckets: StatusBuckets,
    pub oldest_pending_secs: Option<i64>, // None = 无 pending 行（CLI 输出 "n/a" 同语义）
}

pub async fn sample_audit_outbox(pool: &sqlx::PgPool)
    -> Result<AuditOutboxSample, aero_audit_connector::outbox::Error>
pub async fn sample_audit_outbox_full(pool: &sqlx::PgPool)
    -> Result<AuditOutboxFullSample, aero_audit_connector::outbox::Error>
```

实现要点：

- **Tier-1 走 connector 单一路径**：`let repo = aero_audit_connector::pg::PgOutboxRepo::new(pool.clone()); repo.verdict_probe().await?`——SQL 只写一遍（connector），metrics.rs 不复制探针。Q0 在 metrics.rs 本地（R3 谓词镜像 aero-eng `Q0_SQL`）：
  ```sql
  SELECT runtime.enabled,
         (SELECT count(*) FROM snaplink_commercial_bindings WHERE enabled)
    FROM snaplink_commercial_runtime runtime
   WHERE runtime.singleton
  ```
  `sqlx::query_as::<_, (bool, i64)>().fetch_optional(pool)`；**零行 → `(false, 0)`**（与 0239 Gate 1 `COALESCE(enforcement_enabled, FALSE)` fail-open 同语义，FM3）。Q0 Err → 整采样 Err（F1：0235 缺席必须 unverifiable，不冻结）。
- **Tier-2 走 `repo.status_buckets()`** + Q4-mirror：`SELECT extract(epoch FROM (clock_timestamp() - min(available_at)))::bigint FROM audit_governance_outbox WHERE status = 0`（空结果 → `None`）；任一 Err → 整采样 Err → 慢块 warn + 留旧值（**慢路径失败不影响 30s 判定路径**——两档独立失败，FM10）。
- 错误类型统一 `aero_audit_connector::outbox::Error`（`Error::Store(sqlx::Error)` 构造 Q0/Q4 的 sqlx 错误，pub variant 可跨 crate 构造）。
- **纯函数（可单测，truth-check 调用方 = timer block）**：
  ```rust
  /// 探针快照 → AuditSnapshot（v1 桶显式字面量全零——D1/D3；dead 用
  /// 存在性 0/1——verdict() 只测 `> 0`，判定语义字节等价，perf 评审 §4）。
  pub fn snapshot_from_sample(s: &AuditOutboxSample) -> aero_eng::audit_provision::AuditSnapshot
  /// 三值映射：Some(FailClosed)=1 / Some(Consistent|Healthy)=0 / None=2。
  pub fn indicator_value(v: Option<&aero_eng::audit_provision::Verdict>) -> u8
  /// 边沿判定：进入 FailClosed(1) 或 unverifiable(2) 才打 ERROR
  /// （prev=None 首 tick 按 0 处理——首 tick 即 fail-closed/unverifiable
  /// 是事件第一次被观察到，要打；首 tick healthy 不打）。
  pub fn verdict_error_edge(prev: Option<u8>, cur: u8) -> bool
  ```
- `snapshot_from_sample` 的 `g0239` 组装：`table: Some("audit_governance_outbox")`、`pending: s.enqueued`、`claimed: s.claimed`、`delivered: 0`（30s 路径不测 delivered；verdict 不消费它）、`dead: s.has_dead as i64`、`oldest_pending_secs: None`（30s 路径不测——F11 值走慢节奏 gauge）、`dead_rows: Vec::new()`、`priority_landed/class_landed: false`。

### 2.5 aero-server — `crates/aero-server/src/bin/boot/metrics_tasks.rs`（R2+R3）

**30s 判定块**（AI DLQ block :135-166 同款骨架——30s、`MissedTickBehavior::Skip`、共享 cancel token；**无 pre-loop `tick().await` discard**——sampler 模式，首 tick 立即触发（tokio 1.52.3 `interval_at(now)`，§0.1），≤30s 是**变化延迟**不是首次出现，observability (c)）：

```rust
{
    let pool = state.pg.clone();
    let cancel = ai_shutdown.clone();
    // register_help × 5（AUDIT_OUTBOX_STATUS/AUDIT_PROVISION_INDICATOR/
    // AUDIT_PROVISION_SAMPLER_UP 为 Gauge，AUDIT_OUTBOX_SAMPLE_ERRORS_TOTAL
    // 为 Counter——registry 有 inc_counter，§0.1）
    tracker.spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut indicator: Option<u8> = None; // 跨 tick 状态（None = 首 tick 前）
        let mut last_full = AuditOutboxFullSample::default(); // 慢路径缓存（仅慢块写；30s 路径不写 delivered——perf 评审「delivered 不上 30s 路径」）
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = tick.tick() => {}
            }
            match sample_audit_outbox(&pool).await {
                Ok(sample) => {
                    // 桶 gauge：只写本轮实测值（enqueued/claimed 精确、
                    // dead 存在性 0/1）。delivered 不在此写（慢块写）。
                    set_gauge_labeled(AUDIT_OUTBOX_STATUS, sample.enqueued as f64, &[("status", "enqueued")]);
                    set_gauge_labeled(AUDIT_OUTBOX_STATUS, sample.claimed as f64, &[("status", "claimed")]);
                    set_gauge_labeled(AUDIT_OUTBOX_STATUS, if sample.has_dead { 1.0 } else { 0.0 }, &[("status", "dead")]);
                    set_gauge(AUDIT_PROVISION_SAMPLER_UP, 1.0);

                    let v = verdict(&snapshot_from_sample(&sample));
                    let cur = indicator_value(Some(&v));
                    if verdict_error_edge(indicator, cur) {
                        match &v {
                            Verdict::FailClosed(reason) => tracing::error!(
                                verdict = "fail-closed", reason = %reason,
                                dead = sample.has_dead as i64, relay_enabled = sample.relay_enabled,
                                enabled_bindings = sample.enabled_bindings,
                                "audit-provision-check: verdict: fail-closed",
                            ),
                            _ => unreachable!("edge-true in the Ok branch implies cur == 1 (FailClosed); cur == 2 only arises from the Err branch"),
                        }
                    }
                    if let Some(prev) = indicator {
                        if prev != cur && cur == 0 {
                            tracing::info!("audit-provision-check: verdict: recovered (indicator 0)");
                        }
                    }
                    indicator = Some(cur);
                    set_gauge(AUDIT_PROVISION_INDICATOR, cur as f64); // level-triggered：每成功 tick 幂等重写
                }
                Err(e) => {
                    // F1/F2：失败 tick 绝不发布 fabricated 数据——
                    // indicator=2（unverifiable）、sampler_up=0、errors+1；
                    // 桶 gauge 不写（留旧值，诚实：sampler_up=0 标记陈旧）。
                    set_gauge(AUDIT_PROVISION_SAMPLER_UP, 0.0);
                    common_metrics::global().inc_counter(AUDIT_OUTBOX_SAMPLE_ERRORS_TOTAL, 1);
                    let cur = 2u8;
                    if verdict_error_edge(indicator, cur) {
                        tracing::error!(error = %e, "audit-provision-check: verdict: unverifiable");
                    }
                    indicator = Some(cur);
                    set_gauge(AUDIT_PROVISION_INDICATOR, cur as f64);
                }
            }
        }
    });
}
```

- **ERROR 语义（observability (e)1 修正落地）**：ERROR 只在 **0→1**（进 FailClosed）与 **{0,1}→2**（进 unverifiable）边沿各打一次；**1→1 持续态零 ERROR**（gauge level-triggered 承载，`for: 2m` 告警即可）；**1→0/2→0 恢复打 INFO 不打 ERROR**。静态消息保留 greppable 前缀 `audit-provision-check: verdict:`，字段携带 detail（sampler warn 先例；harness grep 的是 CLI 报告非 server 日志，字节一致非必需）。
- **`fresh` 已删除（D6）**：REV 1 草图的 `sample != last` 等值门有双重 bug（`AuditOutboxSample` 无 `PartialEq` = 编译 break；等值永不触发 = indicator 系列永不写；且混淆「查询失败留旧值」与「数据真没变」）。REV 2 用 `Result` 失败语义 + `Option<u8>` indicator 状态——查询失败与数据不变天然可分。
- **30s 路径不写 delivered gauge**（perf 评审：delivered 单调增长、零决策价值；系列由慢块每 300s 刷新，缺失即陈旧标记）。
- **`unreachable!` 臂**：`verdict_error_edge` 为 true 时 cur ∈ {1,2}，Ok 分支里 cur==2 不可能（Err 分支才产生 2）——`match` 的 `_` 臂写 `unreachable!`（clippy 友好：`unreachable_pub` 无关，普通 `unreachable!` 宏）。

**慢节奏全量块**（Tier-2；`AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 默认 300，**0 禁用**——retention.rs:144 同款 env 读取，boot 时读一次）：

```rust
{
    let full_secs = std::env::var("AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS")
        .ok().and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(300);
    if full_secs == 0 {
        tracing::info!("audit outbox full sample disabled (AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS=0)");
        return; // 或跳过 spawn
    }
    // 同款骨架：interval(full_secs)、Skip、共享 cancel token、无 pre-loop discard
    // （首 tick 立即——delivered/oldest_pending 系列秒级出现，之后每 full_secs 刷新）。
    // 每 tick：
    match sample_audit_outbox_full(&pool).await {
        Ok(full) => {
            for (label, n) in [("enqueued", full.buckets.enqueued), ("claimed", full.buckets.claimed),
                               ("delivered", full.buckets.delivered), ("dead", full.buckets.dead)] {
                set_gauge_labeled(AUDIT_OUTBOX_STATUS, n as f64, &[("status", label)]);
            }
            if let Some(secs) = full.oldest_pending_secs {
                set_gauge(AUDIT_OUTBOX_OLDEST_PENDING_SECS, secs as f64);
            } // None（无 pending）→ 不写（系列缺席 = CLI "n/a" 同语义）
        }
        Err(e) => tracing::warn!(error = %e, "audit outbox full sample failed"), // 留旧值；不影响 30s 判定路径
    }
}
```

- 两块的 cancel token 均为既有 `ai_shutdown`（sampler 族惯例）。
- 30s 与慢块**独立失败**：慢块 Err 只是 warn+留旧值（观察面降级）；30s 判定块 Err 才是 unverifiable（门姿态）——分层明确。

### 2.6 connector — `crates/aero-audit-connector/Cargo.toml`（R4）

```toml
[dev-dependencies]
aero-eng.workspace = true
```

（仅 dev-dep；生产依赖树零变化，§0.1 已验证不成环。REV 2 不变。）

### 2.7 connector — `crates/aero-audit-connector/tests/state_machine.rs`（R4，REV 2 扩展）

**新测试 1** `forbidden_dead_surfaces_as_provisioning_gate_fail_closed`（复用 A1-4 装置：`StubSink` events_status:403 + `FakeOutbox` + pin 时钟 + `relay()` helper），六步：

1. 既有 A1-4 断言原样保留：`dispatch_batch()` == 1、`row.status == Dead`、`attempts == 1`、`stub.posts() == 1`。
2. **新增** `fake.verdict_probe()` → `VerdictProbe { enqueued: 0, claimed: 0, has_dead: true }`。
3. **新增** `fake.status_buckets()` → `StatusBuckets { enqueued: 0, claimed: 0, delivered: 0, dead: 1 }`。
4. **新增** 探针快照组装 `AuditSnapshot`（`relay_enabled: true, enabled_bindings: 1, v1: 显式全零字面量, g0239: Some(…pending: 0, claimed: 0, delivered: 0, dead: 1…)`）→ `verdict()` → `matches!(Verdict::FailClosed(reason))` 且 `reason.contains("dead")`。
5. **新增（parity，perf 评审 §4）** 同形状精确快照（`pending: 0, claimed: 0, dead: 1`）→ `verdict()` 结果与探针快照**相同**——探针 `> 0` 语义与精确桶字节等价，AC2-C 同源成立。
6. **新增（只读钉）** `verdict_probe()` + `status_buckets()` 调用前后 `fake.row(id)` 快照逐字段相等（status/attempts/available_at/claim_token/lease_expires_at/last_error）——采样不 mutation。
7. `stub.shutdown()` 收尾（与 A1-4 一致）。

**新测试 2** `verdict_probe_and_status_buckets_track_state_machine`（AC1-A，无 DB）：seed 4 行各状态（insert → claim → settle → delivered；insert → claim → mark_dead → dead；insert → claim 不 settle → claimed；insert → ready）→ `verdict_probe()` == `{1, 1, true}` 且 `status_buckets()` == `{1, 1, 1, 1}`；再跑一轮 claim/settle 后两采样面同步翻转（采样面与状态机一致）。子用例：全 delivered 行集 → `{0, 0, false}` / `{0, 0, N, 0}`（dead-absent + undelivered-absent 分支）。

**新测试 3** `probe_snapshot_matches_cli_oracle_for_arm_shapes`（AC2-C C2，F3 钉，无 DB）：纯 aero-eng 单测风格——对三种 arm 形状（dead 臂 / disabled-with-backlog 臂 / healthy 臂）断言「探针快照 verdict == 精确快照 verdict」；**特别地**：`{enqueued: 0, claimed: 0, has_dead: false, relay_enabled: false, enabled_bindings: 0}` → `Consistent`——与 harness leg B2 的 CLI=FailClosed（同库 switch off + v1 undelivered）构成 **F3 分歧钉**（sampler 侧 0239-only、CLI 侧 v1 臂；分歧是文档化设计，不是 bug，见 §4 FM3′/D3）。

### 2.8 aero-server — `crates/aero-server/src/metrics.rs` `mod tests`（R2，REV 2 新增单测）

纯函数单测（无 DB；metrics.rs 既有 `mod tests` :506 内，`render_prometheus` 断言先例）：

1. `indicator_value_maps_three_states`：`Some(FailClosed)`→1、`Some(Consistent)`→0、`Some(Healthy)`→0、`None`→2。
2. `verdict_error_edge_matrix`：**12 组合全钉**——`(None,0)=false, (None,1)=true, (None,2)=true, (0,0)=false, (0,1)=true, (0,2)=true, (1,1)=false, (1,0)=false, (1,2)=true, (2,0)=false, (2,1)=true, (2,2)=false`。
3. `snapshot_from_sample_arm_coverage`：三分支——`{enqueued:0, claimed:0, has_dead:true, relay_enabled:false, enabled_bindings:0}` → FailClosed(reason 含 "dead")（dead 臂与 relay_enabled 无关）；`{enqueued:2, claimed:0, has_dead:false, relay_enabled:false, enabled_bindings:0}` → FailClosed("no audit:event:write grant issued")（disabled 臂）；`{…, has_dead:false, relay_enabled:true, enabled_bindings:1}` → Healthy。v1 全零字面量断言（D1/D3 编译钉）。

### 2.9 connector — `crates/aero-audit-connector/src/pg.rs` `mod tests`（R1，REV 2 新增 `#[ignore]` db_test）

**新 db_test** `two_tier_probes_match_seeded_distribution_and_are_readonly`（`#[ignore = "requires live Postgres (DATABASE_URL)"]`，`ensure_outbox_table` :276 先例，TRUNCATE 自隔离）：

1. seed 分布 status=0×2 / 1×1 / 2×1 / 3×1 → `verdict_probe()` == `{2, 1, true}` + `status_buckets()` == `{2, 1, 1, 1}`。
2. TRUNCATE + seed status=2×2 → `{0, 0, false}` + `{0, 0, 2, 0}`（dead-absent / undelivered-absent 的 PG 侧分支）。
3. **只读钉**：两探针调用前后全行 `SELECT`（event_id/status/attempts/claim_token/lease_expires_at/last_error/priority/class）逐行相等。
4. **执行计划健康（可选断言，非硬钉）**：`EXPLAIN` 钉 QP1 走 `audit_governance_due_idx`（0239:55）或 `audit_governance_due_prio_idx`（0240:29）——防未来迁移删索引悄悄退化为 seq scan（perf 评审的 index coverage 契约）。

命令：`cargo test -p aero-audit-connector -- --ignored`（throwaway 已迁移库，AGENTS §4.3 一次性库纪律）。

### 2.10 harness — `scripts/b5-pin.sh` + `scripts/test-integration.sh`（AC2-B/AC4，REV 2 新增）

**新 b5_check 槽 `audit-outbox-sampler`**（B5-4 运行期采样冒烟的专属契约槽——CLI 槽 `audit-provision-check` 是 psql 检查面，运行期 sampler 面需要自己的 verdict 证据行）：

- `scripts/b5-pin.sh`：`B5_CONTRACT_TEST_LIST` 在 `audit-provision-check` 后插入 `audit-outbox-sampler`；`assert_b5_contract_pin` 的历史 `-ne 37` → `-ne 38` 计划已被当前 48-slot manifest 取代；当前 runtime sampler 复用既有 slot，不再按本历史草案新增计数。
- `scripts/test-integration.sh`：新增 `AUDIT_SAMPLER_DB="aero_audit_sampler_$$"` + `assert_disposable_db_name` 条目；新段落在 T-11 段之后（0239 文件 gate，缺席 → `b5_check "audit-outbox-sampler" "SKIP (0239 not landed)"`）：

```
▶ 新段（audit-outbox-sampler）：
1. create_throwaway_database → migrate（aero-cli migrate，既有模式）。
2. seed：1 行 status=0（payload 合法 JSON）+ 1 行 status=2（delivered）。
3. 起 server：前台/setsid 后台 + 输出重定向文件（AGENTS §4.3）：
   AERO__DATABASE__URL=$URL AERO__SERVER__BLOB_DIR=/tmp/aero/blobs
   AERO__SERVER__HLS_DIR=/tmp/aero/hls AERO_METRICS_TOKEN=smoke-token
   AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS=60   # 缩短慢周期，冒烟内可见
   （不设任何 AERO_AUDIT_* → relay Ok(None)，sampler 独立采样——E5 钉）
   AERO__NATS__URL / AERO__REDIS__URL 沿用 harness 默认（§0.1）。
   poll 日志直到出现 "spawn_metrics_tasks"（或等首个采样 tick 的 debug/warn）。
4. L1（healthy 基线，≤30s）：curl -H "Authorization: Bearer smoke-token" /metrics →
   aero_audit_provision_indicator 0、aero_audit_provision_sampler_up 1、
   aero_audit_outbox_status{status="enqueued"} 1、{status="claimed"} 0、{status="dead"} 0；
   ≤60s 后（慢块首 tick 立即）{status="delivered"} 1 且
   aero_audit_outbox_oldest_pending_secs 存在；日志 grep -c "verdict:" == 0。
5. L2（403 → 自动 flip）：UPDATE 该 status=0 行 → status=3, last_error='403 provisioning refusal (smoke)'；
   ≤45s（1 tick phase + query time + poll 松弛）→ 日志 `grep -c "audit-provision-check: verdict: fail-closed"` == 1
   （边沿：恰好一次）；/metrics → indicator 1、{status="dead"} 1。
6. L3（恢复，边沿验证）：DELETE 该 dead 行；≤45s → /metrics indicator 0、{status="dead"} 0；
   日志出现 "verdict: recovered"（INFO）；**fail-closed ERROR 计数仍 == 1**（0→1→0 只打一次 ERROR——edge 钉）。
7. L4（F1/F2，unverifiable）：DROP TABLE audit_governance_outbox；≤45s → /metrics
   indicator == 2（**≠ 0**——F2：绝不 fabricated 全零）、sampler_up == 0、
   aero_audit_outbox_sample_errors_total >= 1；日志 `grep -c "verdict: unverifiable"` == 1（边沿）。
8. pkill aero-server（短命跑完必清，AGENTS §4.3）；drop_created_database；b5_check "audit-outbox-sampler" "PASS"。
```

- 冒烟断言全部有**确定性可 grep 产物**：`/metrics` 文本（bearer 门控）+ server 日志（greppable 前缀）。超时上限 45s = 1 tick（30s）+ phase 松弛（observability (d)：bound 是 ≤1 tick + query time，0–30s phase 均匀、均值 ~15s）。
- 该段同时服务 AC1-C（/metrics 四桶可见）与 AC2-B（自动 flip + 边沿 + unverifiable）与 F1/F2 钉。

## 3. Compatibility constraints

| 面 | 约束 | 依据 |
|---|---|---|
| **零改动文件** | `relay.rs`、`client.rs`、`config.rs`、`aero-cli`、`health.rs`/readyz、`aero-common`、migrations（零新）、drill bins（T-11/priority） | E3 的 403→mark_dead 是 T-11 已钉契约；CLI 保持手动 oracle；readyz sibling 钉零接触 |
| **aero-eng** | **零代码改动**：只新增调用方。`V1OutboxCounts` 无 `Default` derive（§0.1）→ sampler 用显式字面量构造（§2.4 `snapshot_from_sample`），不给 aero-eng 加 derive（D1） | req「verdict 函数零改动，只新增调用方」 |
| **trait 扩展** | `OutboxRepo` 加 2 方法 = breaking；实现者恰 2 个（§0.1）→ pg.rs + fake.rs 同步补齐即编译通过；无第三方实现者 | `rg "impl OutboxRepo for"` |
| **生产依赖** | aero-server 零新依赖（aero-eng :30 / aero-audit-connector :36 已有）；connector 仅 dev-dep aero-eng（无环） | §0.1 |
| **relay presence 独立** | sampler 不读 `AERO_AUDIT_TOKEN_ENDPOINT`、不依赖 relay spawn；relay 缺席（`Ok(None)`）时 gauge 照常采样 | E5 + req R2 |
| **表缺席降级（升级语义）** | 0239 未迁移 → 任一 Tier-1 查询 Err → **indicator=2（unverifiable）+ sampler_up=0 + errors+1**（REV 1 的 warn+留旧值冻结 0 被 F1/F2 否决）；**不改 boot 行为、不 crash** | E5 注释 + F1/F2 |
| **gauge 命名/基数** | 名走 aero-server 局部 const（`INDEX_SIZE_BYTES` 先例），不动 `aero-common::metrics::names`；基数固定 4 label 值 + 3 无 label gauge + 1 counter | §2.4 |
| **indicator 更名** | `aero_audit_provision_fail_closed`（REV 1 名）→ `aero_audit_provision_indicator`（三值语义下原名误导；未落地，更名零成本） | observability (b)/D7 |
| **harness（有意的最小变更）** | b5-pin.sh 槽 37→38（新增 `audit-outbox-sampler`）+ test-integration.sh 新段 + `AUDIT_SAMPLER_DB` 变量——REV 1 的「零 harness 改动」因新增运行期契约而修订；其余槽/leg 零改动 | §2.10 |
| **lint/门禁** | 不新增 clippy 警告（workspace `all`+`pedantic` warn）；`unsafe_code = forbid`；truth-check 0 违规（新函数都有调用方：timer 块调 5 采样/纯函数，测试调 3 纯函数） | AGENTS §4.2 |
| **MSRV/风格** | Rust 2021 / 1.80；`#[allow(clippy::cast_precision_loss)]` 沿用 AI DLQ 块既有写法 | workspace |

## 4. Failure modes（含降级语义，REV 2 修订）

| # | 故障 | 行为 | 降级 |
|---|---|---|---|
| FM1 | 0239 表缺席（pre-B5-1 boot） | Tier-1 `verdict_probe` → `Err(UndefinedTable)` → 整采样 Err → **indicator=2（unverifiable）、sampler_up=0、errors+1、桶 gauge 不写（留旧值）、ERROR「unverifiable」边沿一次**（首 tick 即 2 → 打一次；之后 2→2 不打） | F13 同款服务器正常起；**语义升级**：pre-0239 库显式 unverifiable，**不再 looks healthy-empty**（F2）。「首 tick = 无系列」是**错误 gap 非时序 gap**——首 tick 立即触发（observability (c)） |
| FM2 | Q0 查询失败（0235 未迁移/DB 瞬时错） | Q0 Err → 整采样 Err → 同 FM1 消化路径（indicator=2…） | **F1 修复**：持久 constituent-query 失败（0235 丢、0239 在）不再冻结 0——unverifiable 持续 surface，relay 继续 mark_dead 也会被 2 态盖住（2 是「不可验证」不是「健康」） |
| FM3 | Q0 零行（`runtime.singleton` 行不存在） | `fetch_optional` → `None` → 按 `(false, 0)` 处理（与 0239 Gate 1 `COALESCE(..., FALSE)` 同语义） | relay 视为 disabled；若 0239 无 undelivered → `Consistent`（indicator 0）。**生产不可达**——0235 `INSERT … ON CONFLICT DO NOTHING` 保证 singleton 行（PK+CHECK；仅手动 DELETE 可达零行）。CLI 对同库 `parse_relay_line` 空输出是 fail-loud Err——两 oracle 差异保留（CLI=检查，sampler=观察面） |
| FM3′ | **F3（评审钉）**：switch off + v1 `snaplink_delivery_outbox` undelivered audit 行（0236 触发器对**所有** action 入 v1，含 `message.moderated`，无 token-keyed 排除） | 采样快照 v1 全零（D3）→ `Consistent`(0)；CLI 读 v1 臂 → `FailClosed`。**可达的同库分歧，文档化 + 钉住（AC2-C C2 + harness leg B2）** | 设计既定（运行期循环 0239-only，v1 臂属 CLI/boot 门 oracle）；indicator 不评估 v1——**不是 bug 是契约**（§5 声明已改）；若未来要全同义：慢节奏读 v1（成本未分析，越界，见 §8） |
| FM4 | 403 → dead 行出现 | 下一 tick `has_dead=true` → `verdict()` FailClosed（dead 臂优先，与 relay_enabled 无关）→ **ERROR 一次（0→1 边沿）** + indicator 1 + `{status="dead"}=1` | **延迟精度**：sampler 侧 ≤1 tick + query time（phase 均匀 0–30s、均值 ~15s；Skip 无 catch-up，普通负载下 bound 成立，持续饥饿任何 timer 都无界）；dead 行本身要等 relay poll（默认 `AERO_AUDIT_POLL_INTERVAL_SECS`=5s，config.rs:76）+ 403 round-trip——端到端 = relay poll ≤5s + RTT + sampler 侧。**`{status="dead"}>0` ⟺ dead 臂**（等价性文档化，observability (a)6） |
| FM5 | dead 行被清（运维删除/重放修复） | 下一 tick `has_dead=false` → indicator 0 + **INFO「recovered」**（不打 ERROR——边沿验证靠 ERROR 计数不变）+ dead gauge 0 | 无迟滞（flap 可能 toggle）；indicator 语义明确；恢复清除延迟 ≤1 tick（ops-driven DELETE） |
| FM6 | DB 瞬时错（30s 路径） | 整采样 Err → unverifiable + errors+1；下 tick 自愈 → indicator 回到 0/1 + INFO「recovered」 | 与 AI DLQ block 同款容错；快照两查询顺序执行可 straddle 变更——verdict 臂对 skew 鲁棒（dead 臂不依赖 Q0；disabled 臂需 backlog），瞬时误判下 tick 自愈（观测评审 (d)4，无需代码） |
| FM7 | 多实例（水平扩） | 每实例独立采样同库；N 系列同值、phase 差 ≤1 tick；**聚合契约：`max()`**（indicator + 各 bucket——fail-safe stale-high；`sum()` 对 boolean 无意义、`avg()` 错误；健康实例不能掩盖 fail-closed 实例） | 只读面天然水平扩；重启后系列消失窗口 ≈ 1 query RTT（首 tick 立即）且**值可从 DB 重派生**（gauge-over-counter 的决定性论据，D5） |
| FM8 | 采样面被误加写 | T-11 drill `COUNT/SUM` 断言 + R4 只读钉即红——**drill 是 no-mutation 守卫** | 回归面（AC4） |
| FM9 | gauge 名冲突 | `register_help` 幂等（matching kind 覆盖 HELP）；kind 冲突静默忽略（`gauge_family` 语义） | 无 panic（registry 设计） |
| FM10 | 慢节奏全量块失败（表在 30s 探针 OK 后短暂不可用等） | `sample_audit_outbox_full` Err → warn + 留旧值；**不影响 30s 判定路径**（两档独立失败） | delivered/dead/oldest_pending 系列陈旧（sampler_up 仍 1——up 语义 = 判定路径成功）；下慢 tick 自愈 |
| FM11 | relay 缺席 + switch on / transient-forever blackhole（healthy-with-zero-progress） | `oldest_pending_secs` gauge（慢节奏）持续增长/存在——zero-progress 可见（F11 关闭盲区）；indicator 本身按 CLI 继承语义不报（设计既定） | 观察面修复；verdict 语义不动（CLI-inherited，文档化） |

## 5. Migration steps

**零新迁移**——0239 已含全部所需列（status/last_error/时间戳），0235 已含 Q0 事实源。本设计的「迁移」是**部署时序**：

1. **提交前置**（AGENTS §4.1 与 batch 纪律）：当前工作树 B5 产物（governance.rs、audit_provision.rs(+tests)、aero-audit-connector/、migrations 0239-0241 等）**未提交**——先归组提交，否则 `git reset --hard master` 抹掉本设计全部验收对象。
2. **代码落地顺序**：R1（connector 两档探针 + 三处实现 + PG db_test）→ R4（state_machine 全链测试，connector 内闭环、无服务依赖）→ R2+R3（metrics.rs + metrics_tasks.rs + 纯函数单测）→ harness 槽/冒烟段（b5-pin.sh 38-slot bump + test-integration.sh 新段）→ 冒烟 → 全链门禁（§7）。
3. **无 schema 变更**：不需要 `cargo build` 后 migrate 的重排序（无新 `migrations/` 文件）；已有部署的 0239 表直接可用。
4. **旧版本兼容**：新 sampler 在 0239 缺席的旧库上按 FM1 降级（indicator=2——**升级观察面：升级后 pre-0239 实例首次出现 2 属预期，不是故障**）；在 0235 缺席的库上按 FM2 降级——**向后兼容，无回滚面**（回滚 = 移除 timer block + 槽位还原，纯代码）。
5. **观察面上线顺序**：gauge 先于告警接线；`aero_audit_provision_indicator` 出现 1 前先验证四桶基线（基线期 0 属预期）；**告警规则按 `max()` 聚合 + `indicator == 1` 持续 `for: 2m`**（拒绝 transitions counter，D5）；`indicator == 2` 告警需按部署阶段排除 pre-0239 实例（FM1 文档化）。
6. **同库同义声明（REV 2 修订，F3）**：~~「两 oracle 同库同义以有 runtime 行的生产形态为准」~~ → **「同库同义仅指 0239 桶判定语义（探针 `> 0` ⟺ CLI Q3 精确桶 `> 0`，AC2-C C2 钉）；v1 臂是 CLI/boot 门专属 oracle，运行期循环 0239-only（D3）——switch off + v1 undelivered 时 CLI=FailClosed 而 sampler=Consistent(0) 是**可达的、文档化的**分歧（FM3′），由 AC2-C C2 + harness leg B2 双向钉住」**。

## 6. Testable acceptance mapping（req AC1–AC4 → 可测断言，REV 2 重钉）

| AC（req 原句片段） | 可测断言（Oracle） | 落点 | 命令 |
|---|---|---|---|
| **AC1** Boot timer samples `audit_governance_outbox` status buckets into Prometheus gauges (pending/claimed/delivered/dead) with no state mutation | **A（无 DB，fake 双面）**：seed 4 状态各 1 行 → `verdict_probe()` 精确 `{1,1,true}` + `status_buckets()` 精确 `{1,1,1,1}`；跑 `claim_due`/`settle`/`mark_dead` 后两采样面同步翻转；全 delivered 子集 → `{0,0,false}`/`{0,0,N,0}`（dead/undelivered absent 分支） | state_machine.rs 新测试 2（§2.7） | `cargo test -p aero-audit-connector --test state_machine` |
| | **B（PG-gated 双面）**：`ensure_outbox_table` + seed `{0×2,1×1,2×1,3×1}` → probe `{2,1,true}` + buckets `{2,1,1,1}`；TRUNCATE + `{2×2}` → `{0,0,false}`/`{0,0,2,0}`；**行集逐行不变**（SELECT 前后全字段等）；EXPLAIN 钉 QP1 走 due 部分索引（防 seq scan 回归） | pg.rs `#[ignore]` db_test（§2.9） | `cargo test -p aero-audit-connector -- --ignored`（throwaway 已迁移库） |
| | **C（sampler /metrics）**：throwaway 已迁移库 seed `{1 ready, 1 delivered}` → 起 server（`AERO_METRICS_TOKEN` + 慢节奏 60s）→ `/metrics`（bearer）可见 `aero_audit_outbox_status{status="enqueued"} 1`、`{status="claimed"} 0`、`{status="dead"} 0`（30s 存在性）、`{status="delivered"} 1` + `aero_audit_outbox_oldest_pending_secs`（慢块，≤60s）+ `sampler_up 1` + indicator 0；日志零 `verdict:` 行 | harness 新段 L1（§2.10） | 前台/pkill 纪律（AGENTS §4.3） |
| | **D（运行态无 mutation）**：T-11 DB 上跑 sampler → `COUNT(*)`/`SUM(attempts)` 前后相等 | 与 AC4 同源（drill 断言）+ 新测试 1 步骤 6 只读钉 | `bash scripts/test-integration.sh` |
| **AC2** relay-health signal (dead-count) consumed by the provisioning gate so a 403 → dead row flips verdict to FailClosed automatically, not only when the CLI runs | **A（全链单测，六步）**：403 → Dead → `verdict_probe` `{0,0,true}` → `status_buckets` `{0,0,0,1}` → 探针快照 `verdict()` `FailClosed`（reason 含 "dead"）→ **parity**（精确快照同 verdict）→ 只读钉 | state_machine.rs 新测试 1（§2.7） | `cargo test -p aero-audit-connector --test state_machine` |
| | **B（运行态自动 flip + 边沿 + unverifiable）**：throwaway 已迁移库（配或不配 `AERO_AUDIT_TOKEN_ENDPOINT` 均可——sampler 独立于 relay presence）→ **L2**：seed dead 行 → ≤45s 日志 `audit-provision-check: verdict: fail-closed`（ERROR）**恰 1 次**（`grep -c` == 1，边沿）+ indicator 1 + dead gauge 1；**L3**：DELETE → indicator 0 + INFO recovered + **ERROR 计数仍 1**；**L4**：DROP TABLE → indicator **2（≠0）** + sampler_up 0 + errors_total ≥ 1 + `verdict: unverifiable` ERROR 恰 1 次 | harness 新段 L2-L4（§2.10） | 短命跑完 `pkill aero-server` |
| | **C（CLI 同源 + F3 分歧钉）**：**C1**：healthy/disabled/dead 形状下 CLI 与 sampler 探针对同库判定一致（`> 0` 语义字节等价，parity 钉）；**C2（F3）**：switch off + v1 undelivered → CLI=FailClosed（harness leg B2 已钉，`verdict: fail-closed` + `no audit:event:write grant issued`）而探针快照=Consistent（新测试 3）——**分歧显式化** | 新测试 3 + 既有 leg B2 | `cargo test -p aero-audit-connector --test state_machine` + `bash scripts/test-integration.sh` |
| **AC3** extend state_machine.rs with a test asserting mark_dead on 403 surfaces as a provisioning-gate fail-closed condition | 新测试 1/2/3 全绿（六步链 + 状态机跟踪 + parity/F3）；既有 `forbidden_dead_on_first_attempt` 保持绿（断言零改动） | state_machine.rs | `cargo test -p aero-audit-connector --test state_machine` |
| **AC4** T-11 and aero-audit-priority-drill stay green | **A**：`b5_check "t11-fail-closed" "PASS"` + `b5_check "moderation-priority-drill" "PASS"` + **`b5_check "audit-outbox-sampler" "PASS"`**（**新槽**：b5-pin.sh 37→38，§2.10；0239 缺席 → 新槽 SKIP 同既有 gate） | scripts/b5-pin.sh + test-integration.sh（T-11 段后新段） | `bash scripts/test-integration.sh` |
| | **B**：drill bins 直跑 exit 0（throwaway 库） | 零改动保持 | `cargo run -p aero-audit-connector --bin aero-audit-t11-drill`（及 priority-drill） |
| | **C**：`cargo test --workspace --lib` 全绿 + `aero-eng` 22/22 绿（verdict 零改动）+ `cargo clippy --workspace --all-targets` 无新警告 | 回归 | `cargo test --workspace --lib` |

### 6.1 分支覆盖矩阵（REV 2：每个新分支都有 oracle 钉）

| 新分支 | 覆盖 oracle（§6 行） |
|---|---|
| `verdict_probe` Ok：undelivered 存在 / 缺席 | AC1-A（fake 双种子）、AC1-B（PG 双分布）、AC1-C |
| `verdict_probe` Ok：dead 存在 / 缺席 | 同上（`has_dead` true/false 各至少一处） |
| `verdict_probe` Err（0239 缺席） | AC2-B L4（DROP TABLE → 整采样 Err） |
| `status_buckets` 四桶精确（含 delivered/dead 非零） | AC1-A/B、AC1-C（delivered=1） |
| `sample_audit_outbox` Ok / Err 双路径 | AC1-C（Ok）、AC2-B L4（Err） |
| `sample_audit_outbox_full` Ok / Err（慢块留旧值） | AC1-C（Ok）、AC2-B L4（Err 后 30s 路径仍工作 = 两档独立，FM10 语义） |
| `snapshot_from_sample` 三分支（dead 臂 / disabled 臂 / healthy 臂） | metrics.rs 单测 3（§2.8） |
| `indicator_value` 0/1/2 | metrics.rs 单测 1 |
| `verdict_error_edge` 12 组合 | metrics.rs 单测 2（全矩阵） |
| 定时循环：无 pre-loop discard（首 tick 立即） | AC2-B（秒级出现断言 + L1 无 `verdict:` 行） |
| 边沿 ERROR 0→1 恰一次（持续 1→1 零 ERROR） | AC2-B L2（`grep -c` == 1） |
| 恢复 1→0 无 ERROR（INFO recovered） | AC2-B L3（ERROR 计数仍 1） |
| 失败 tick → indicator=2 + sampler_up=0 + errors+1 + 桶 gauge 不写 | AC2-B L4 |
| 进 unverifiable 边沿 ERROR 恰一次（2→2 不重复） | AC2-B L4（`grep -c` == 1） |
| 慢节奏：四桶精确 + oldest_pending 写面；env=0 禁用 | AC1-C（缩短 env=60 可见）；env=0 禁用路径 = boot info 日志（文档断言，非硬钉） |
| 探针 ⟺ 精确桶判定 parity | AC2-A 步骤 5、AC2-C C1 |
| F3 分歧（CLI FailClosed vs sampler Consistent） | AC2-C C2 + 既有 leg B2 |
| 只读钉（采样不 mutation） | AC1-B（PG 行集全等）、AC2-A 步骤 6（fake 行快照全等）、AC4-A（drill 计数守卫） |
| `AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 读取/0 禁 | AC1-C（=60 生效）；0 禁用 = boot 日志断言 |

### 6.2 钉文件完整性核对（D1/D6 编译修复不破坏钉测试）

| 钉文件 | 变更 | 核对结论 |
|---|---|---|
| `crates/aero-eng/tests/audit_provision.rs`（22 测试） | **零改动** | aero-eng 零 diff（D1：sampler 侧显式字面量，不加 `Default` derive）；`verdict()` 签名不变 |
| `tests/state_machine.rs` 既有 A1-1..A1-7（含 `forbidden_dead_on_first_attempt`） | **零改动**（断言逐字保留） | trait 新增方法为增量（既有 5 方法签名零变化）；新测试 1 在其上叠加 |
| `tests/claim_validation.rs`（320 行） | **零改动** | 同上（trait 增量不影响既有调用） |
| `pg.rs` `mod tests` 既有 db_tests | **零改动** + 新增 1 个 `#[ignore]` | `ensure_outbox_table`/TRUNCATE 自隔离先例沿用；`--test-threads` 约定不变 |
| 新测试构造 `VerdictProbe`/`StatusBuckets`/`AuditOutboxSample`/`AuditOutboxFullSample` | derive(`Default`, `PartialEq`) 齐备 | `::default()` 与 `assert_eq!` 编译通过（D6：类型自带 derive，不再依赖 `fresh` 等值比较） |
| **`fresh` 引用** | **全仓删除**（REV 1 仅 §2.5 草图有；无测试文件引用过） | 落地时 `rg fresh crates/aero-audit-connector crates/aero-server` 零命中；`sample_audit_outbox` 返回 `Result`（F2/D6） |
| `scripts/b5-pin.sh` / `test-integration.sh` 既有槽与 leg | 历史草案曾计划新增 1 槽（37→38，§2.10）；当前 manifest 已演进为 48 槽（27 个可执行 + 21 个 [PROPOSED]），runtime sampler 不再改变该计数 | 当前 48-slot guard 与 `b5_check` 协议不变；`t11-fail-closed`/`moderation-priority-drill`/`audit-provision-check` 断言逐字保留 |
| `crates/aero-server/src/metrics.rs` 既有 `mod tests` | 增量（3 个新单测）；既有断言零改动 | 纯函数单测不触 PG |

## 7. Sequencing

1. **提交 batch 产物**（§5 step 1，最高风险前置）。
2. **基线确认**：`cargo test -p aero-eng --test audit_provision`（22/22）· `cargo test -p aero-audit-connector` · `bash scripts/test-integration.sh` T-11/priority legs。
3. **R1**：`VerdictProbe`/`StatusBuckets` + trait 2 方法 + pg.rs/fake.rs 实现 + PG db_test（AC1-A/B，§2.7 测试 2 + §2.9）。
4. **R4**（connector 内闭环，先行）：dev-dep aero-eng + state_machine.rs 新测试 1/3（AC2-A/AC3 + parity/F3）。
5. **R2+R3**：metrics.rs const + 两采样函数 + 3 纯函数 + 单测（§2.8）→ metrics_tasks.rs 30s 块 + 慢块（§2.5）。
6. **harness**：b5-pin.sh 38-slot bump + test-integration.sh `AUDIT_SAMPLER_DB` + 新段（§2.10）。
7. **运行态冒烟**（AC1-C/AC2-B）：L1→L4 全腿（healthy 基线 → 403 flip → 恢复边沿 → unverifiable）；`grep -c` 计数断言 + /metrics bearer curl。
8. **全链门禁**：`cargo check --workspace` · `cargo test --workspace --lib` · `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` 0 违规 · `bash scripts/test-integration.sh`（AC4，38-slot pin）。

## 8. Risks / 决策点

- **D1 — `V1OutboxCounts::default()` 编译不过（req 笔误，REV 1 已修）**：本设计沿用显式字面量 `{ pending: 0, claimed: 0, delivered: 0 }`（§2.4 `snapshot_from_sample`），**aero-eng 零改动**；新类型（`VerdictProbe`/`StatusBuckets`/`AuditOutboxSample`/`AuditOutboxFullSample`）全部自带 `Default`+`PartialEq` derive（测试钉，D6）。
- **D2 — 「automatic runtime-switch flip」否决（沿用 req）**：翻转 `snaplink_commercial_runtime.enabled` 会连带禁用 0235 v1 outbox/usage 等无关 enforcement；fail-closed 的自动面 = ERROR 日志 + indicator gauge + 运行期 verdict 评估。
- **D3 — 运行期快照 v1 桶置零（REV 2 修订）**：v1 `snaplink_delivery_outbox` 是 CLI/boot 门的评估面；运行期循环只管 0239 桶 + dead 臂。**F3 采纳**：0236 v1 触发器对所有 action（含 `message.moderated`，无 token-keyed 排除）入 `destination='audit'` 行 → switch off + v1 undelivered 时 CLI=FailClosed、sampler=Consistent(0) 是**可达同库分歧**——已文档化（FM3′）+ 双向钉住（AC2-C C2 + leg B2）。CLI 仍是 v1 臂 oracle。
- **D4 — 表缺席降级（REV 2 升级：fail-open → unverifiable）**：FM1/FM2 不再 warn+留旧值冻结 0，而是 indicator=2 + sampler_up=0 + errors+1 + 边沿 ERROR——**fail-closed 门姿态**（F1/F2 采纳）；永不 panic。
- **D5 — 无迟滞 + 拒绝 transitions counter（REV 2 扩展）**：gauge 随 dead 行出现/清除即时翻转（≤1 tick）；sticky-dead 使 flapping 不可能，`for:` 子句告警即正确原语。**transitions counter 显式拒绝**：多实例 `sum()` N 倍重复、restart 重置事件史（gauge 从 DB 一查询重派生）——翻转检测 = PromQL `changes()`/`max_over_time`。**聚合契约 = `max()`**（indicator + buckets，fail-safe stale-high，FM7）。**self-hiding 盲区文档化**：enabled + bindings=0 + 空 outbox → Gate 2 RAISE 阻止 enqueue → verdict Consistent——indicator 是 dead+backlog 信号，非完整阻断探测器（boot 门才是，observability (e)2）。
- **D6 — `fresh` 删除（REV 2）**：REV 1 的 `sample != last` 是编译 break（无 `PartialEq`）+ 逻辑死（等值永不触发 → indicator 系列永不写）+ 语义混淆（查询失败留旧值 vs 数据真没变）。REV 2：`sample_audit_outbox() -> Result<…>`（Err → None 语义）+ `Option<u8>` indicator 状态——**每条成功 tick 都评估 verdict，失败显式 unverifiable**。req R2 的 `-> AuditOutboxSample` 签名随评审修正为 `Result<…>`（落地以设计为准）。
- **D7 — 三值 indicator（REV 2 新增）**：`aero_audit_provision_indicator` 0/1/**2=unverifiable**——布尔从「evaluated: not fail-closed」静默重定义为「not evaluated」的问题被消除；`== 1` 告警不受 2 影响；`== 2` 在合法 pre-0239 部署上恒真（那是 F13 窗口的真实状态，按部署阶段排除，FM1 文档化）。更名自 REV 1 的 `fail_closed`（三值下原名误导；未落地更名零成本）。
- **D8 — 边沿/电平分离（REV 2 新增）**：ERROR = 0→1 与进 2 的边沿（`verdict_error_edge` 纯函数，12 组合单测）；gauge = level-triggered 每成功 tick 幂等重写。持续 fail-closed 不再 2880 ERROR/天（observability (e)1）。
- **D9 — 两档 SQL 探针（REV 2 新增，perf 采纳）**：30s 判定路径 = partial-index 计数 + EXISTS（成本 ∝ due 集）；全量聚合移到慢节奏（`AERO__SERVER__AUDIT_OUTBOX_FULL_SAMPLE_SECS` 默认 300、0 禁），摊销 10×；delivered 不上 30s 路径。**诚实 caveat**：dead 存在性在 dead=0 时是一次 bounded heap pass（零迁移不可消除）；根因 = 0239 无 sweeper（终态行永久累积，任何 O(表) 查询都随部署年龄退化）——**sweeper/(status) 索引越界**（写路径迁移），indicator 语义不依赖表大小。
- **D10 — sampler_up gauge（REV 2 新增，observability (b)4）**：`aero_audit_provision_sampler_up`（1=判定路径本轮成功）取代 FM1 上不可用的 `absent()` 告警；配合 `errors_total` counter（per-instance 本地错误，`sum()` 语义正确——与 D5 拒绝的 transitions counter 不同类）。
- **D11 — `oldest_pending_secs` gauge（REV 2 新增，F11）**：慢节奏 Q4-mirror（`aero_audit_outbox_oldest_pending_secs`），None → 系列缺席（CLI "n/a" 同语义）；关闭 transient-forever blackhole / relay-absent-with-switch-on 的 zero-progress 盲区。
- **D12 — 探针 ⟺ 精确桶 parity（REV 2 新增，perf §4）**：`verdict()` 只消费 `> 0` 谓词 → 30s 路径用存在性/计数探针与 CLI 精确桶**判定语义字节等价**，reason 字符串在 30s 路径用存在性数值（dead=0/1；精确量走慢节奏 gauge 与 CLI）——parity 钉在 AC2-A 步骤 5 / AC2-C C1。
- **最高风险：batch 产物 untracked**——提交是 AC1-AC4 前提（§5 step 1）。
- **与 sibling boot-gate spec 边界**：boot 门（未落地）= 启动期一次性 fail-loud；本设计 = 运行期持续评估（**indicator 目前零消费者，ops-signal only**，§1/§4 显式声明）。若 sibling 后续落地，`snapshot_from_sample` 的快照组装可被 boot 门复用（同函数，不重复 SQL）；v1 臂分歧（F3）届时由 boot 门覆盖。
- **改动面总结**：connector = `outbox.rs` + `pg.rs` + `fake.rs` + `Cargo.toml`（dev-dep）+ `tests/state_machine.rs`；aero-server = `metrics.rs` + `metrics_tasks.rs`；harness = `b5-pin.sh`（37→38 槽）+ `test-integration.sh`（新段 + `AUDIT_SAMPLER_DB`）；**relay.rs/client.rs/config.rs/main.rs/CLI/readyz/aero-common/migrations 零改动**。
