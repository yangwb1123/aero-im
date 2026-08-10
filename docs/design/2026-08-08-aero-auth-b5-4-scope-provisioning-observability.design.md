# Design — B5-4 scope provisioning 生产可观测性（fail-closed gate）：warn 级/变化检测日志 + Prometheus gauge（observability_gauge_samplers 模式）+ Q0 免缓存裁决 + priority-drill 门禁接线

- **Base design**: `docs/design/2026-08-08-aero-auth-b5-4-scope-provisioning.design.md`（本设计是其**可观测性增量**，不重述 API/接线主体；§5 只列增量）
- **Requirements**: `docs/requirements/2026-08-08-aero-auth-b5-4-scope-provisioning.req.md`（R1–R6, AC1–AC5）
- **Status**: Design（security review 残余风险收口；全部锚点 2026-08-08 现场复核）
- **Verification date**: 2026-08-08。行号为复核锚点，会漂移——**文件/符号**为准（AGENTS.md §0）

## 0. 任务与证据复核

安全评审的残余风险：**生产 relay 路径在未配给时每个 tick 只有一条 `debug!`，零 claim 静默**——Q0 DB 故障与合法禁用日志不可区分；无 Prometheus 信号；priority-drill 手动-only。本设计逐项裁决：

| # | 交付物 | 裁决摘要 |
|---|---|---|
| T1 | provisioner Err 上 warn 级/变化检测日志，Q0 DB 故障 vs 合法禁用可区分 | §1：`PgRelayScopeProvisioner` 单一日志所有权，转换驱动（transition-driven）日志 + 300s 持续 Err 重告警节流；repo 返回事实 `(enabled, enabled_bindings)` 使文案可区分 |
| T2 | Prometheus gauge 对齐 `observability_gauge_samplers` 模式，未配给零 claim 生产可见 | §2：provisioner 维护共享快照（非 async），30s sampler 读快照设 4 个 gauge；**sampler 零 DB 查询** |
| T3 | 每 `dispatch_batch` 单例 Q0 查询免缓存裁决（或给 TTL） | §3：**免缓存**——正结果缓存会破坏 fail-closed-on-DB-error（正是本 campaign 加的属性）；负结果缓存延迟 ≤1-tick 收敛；error-aware 缓存零收益 |
| T4 | `aero-audit-priority-drill` 覆盖接线（test-integration.sh）或文档化手动门禁命令 | §4：**两者都给**——脚本内 `cargo build` + `AERO_PRIORITY_DRILL_BIN` 覆盖（使 bin 名文本可 grep）+ CLI 缺失时直跑 fallback；另给手动命令 |

### 0.1 锚点复核（2026-08-08 现场）

| 锚点 | 实况 |
|---|---|
| `relay.rs` `dispatch_batch` :164（reconcile :166-168 → `claim_due` :171 → `for_each_concurrent deliver_claim` :176）；门插在 reconcile 之前 | ✅ 精确；`run()` tick 循环 `warn!(?error, "audit relay claim failed…")` :128；`deliver_claim` Forbidden 臂 :201-216 |
| `RelayConfig::from_env` poll_interval 默认 5s、min 1s、max 300s | ✅ `config.rs:88`（`AERO_AUDIT_POLL_INTERVAL_SECS`）——Q0 频次上界由此约束 |
| `Q0_SQL` = aero-eng 私有 const（`audit_provision.rs:34`，无 `pub`） | ✅ 精确；语义 = `runtime.enabled` ∧ enabled bindings > 0 |
| 0235 DDL：`snaplink_commercial_runtime` singleton BOOLEAN PRIMARY KEY（:8-11）；`snaplink_commercial_bindings` PK workspace_id、`enabled` 无索引 | ✅ 精确——Q0 = 单行 PK 查找 + 小表 count |
| `observability_gauge_samplers` 三任务（DB 池+WHIP 15s / AI DLQ 30s / NATS backlog 30s） | ✅ `boot/metrics_tasks.rs`：`register_help` + `set_gauge(_labeled)` + `MissedTickBehavior::Skip` + cancel 即 break；query Err → `warn!` 留旧值 |
| 既有 warn 先例：`pat.rs:46`（"PAT verification query failed"——存储查询失败 warn）、`oidc.rs:762` | ✅ aero-auth 已有 `tracing` 依赖（Cargo.toml:25）——provisioner 落 aero-auth 无新依赖 |
| `main.rs`：`services.auth` move :185；relay 臂 :245-269（`AuditRelay::new` :259）；`spawn_metrics_tasks` :270-277 | ✅ 部分 move 后其他字段仍可读（`services.ai_service` :240 先例）→ sampler 传参可行 |
| `AuditRelay::new` 构造面 | ✅ 实为 **7 处**（main.rs:259 + 3 drill：relay-drill:127 / t11-drill:155 / priority-drill:203 + relay.rs 内部 :374/:565 + state_machine.rs:77）；relay.rs:374 直呼 `deliver_claim` 绕过门，无需注入 |
| priority drill：构造 :203、`dispatch_batch` :208/:249、SKIP 行 :99-103/:129-133、PASS 行 :244/:279/:297（`drill: …: PASS`） | ✅ 退出码 0=全 PASS / 2=SKIP / 1=FAIL（bail） |
| `run_priority`（`aero-eng/src/audit_provision.rs`）spawn drill：`AERO_PRIORITY_DRILL_BIN` 覆盖 + `DATABASE_URL` 注入 + 退出码透传（0/1/2） | ✅ 精确——这是 test-integration.sh 接线的现成 seam |
| `test-integration.sh`：A3 relay drill :320-341、T-11 drill :343-380（含 `B5_HELP_OUT` 存在性检测 :368-379）、moderation-priority drill :441-484（走 aero-eng CLI，bin 名不出现在脚本） | ✅ 精确——T-11 节已有 CLI 存在性 grep 先例，priority 节复用 |
| 符号撞名：`ProvisionState`/`provision_snapshot`/`RelayProvisionSnapshot`/`SharedProvisionState`/`aero_audit_*` 度量名 | ✅ 全仓零命中（含 sibling 设计）——无撞名 |
| aero-auth 现有 `AtomicU8`/`AtomicBool` 共享状态 | ✅ 无先例，但 `aero-common::metrics` 的 atomics + 本设计用 `std` 原子，无新依赖 |

## 1. 日志规范（T1）：Err → warn、转换检测、DB 故障 vs 合法禁用可区分

### 1.1 单一日志所有权

provisioner（`PgRelayScopeProvisioner`）是门关闭原因的**唯一日志所有者**：它看得到原始 `Err` 与 `(enabled, bindings)` 事实，且 relay 的 `dispatch_batch` 与未来的 `assert_audit_scope_provisioned` 两条调用路径都汇入同一检查点——日志天然去重。relay 侧门命中保持 `debug!`/tick（rate 受 `poll_interval ≥ 1s` 约束），只做状态富化（§1.5）。

### 1.2 repo 返回事实而非 bool（可区分性的基础）

`RelayScopeRepo::q0_provisioned()` 返回 `Result<(bool, i64)>` = `(runtime.enabled, enabled bindings count)`（Q0_SQL 两列本来就查出来了，折叠成 bool 再展开是丢信息）。provisioner 映射 `Ok((true, n)) if n > 0` → provisioned，其余 → disabled。日志由此区分三态：**enabled=false**（runtime 开关关）vs **enabled=true 但零 bindings**（授权未发）vs **Err**（DB 故障/表缺失）。base design §2.2 的 `-> Result<bool>` 签名是本设计的增量修改点（新符号，零兼容成本）。

### 1.3 转换驱动日志状态机（纯函数，可单测）

每次检查后按 **前值 → 新值** 转换决定日志（前值来自共享状态 §2.1）：

| 转换 | 级别 | 日志内容 | 理由 |
|---|---|---|---|
| 任意 → `Error` | **`warn!`** | `error = ?e` + "audit relay scope check failed (Q0 DB error); gate fail-closed — zero claims while unverified" | DB 故障 = 需要人介入的异常；`error` 字段带 `UndefinedTable`/连接失败/超时的具体差异 |
| `Error` → `Error`（持续） | `warn!` **≤1 次/300s**（节流），其余 tick `debug!` | 同上 | 避免 5s/tick 刷屏（17k 行/天 → ≤288 行/天） |
| `Provisioned` → `Disabled` | **`warn!`** | `enabled`、`enabled_bindings` + "audit:event:write provisioning revoked — zero claims" | 运行中劣化 = 运维误操作或授权吊销，值得告警 |
| `Unknown`/`Error`/`Disabled` → `Provisioned` | `info!` | `enabled_bindings` + "audit:event:write provisioned — claims resume" | 恢复信号 |
| `Unknown` → `Disabled`（boot 即禁用） | `info!`（一次） | 同上 | **合法禁用的正常形态**（商业 runtime 关 = 绝大多数部署的常态），不 warn |
| `Disabled` → `Disabled`（稳态） | `debug!`/tick | 同上 | 常态，debug 级可见不刷屏 |
| `Provisioned` → `Provisioned`（稳态） | 无 | — | 健康静默 |

**可区分性**（评审要求逐字落实）：Err 行必有 `error=` 字段且文案含 "check failed"；Disabled 行必有 `enabled`/`enabled_bindings` 字段且文案含 "not provisioned"；两行都带 `state=` 字段（`ProvisionState` 判别值）。grep `"audit relay scope check failed"` = DB 故障；grep `"audit relay scope not provisioned"` = 合法禁用。

实现为**纯函数** `fn next_log(prev: Option<ProvisionState>, next: ProvisionState, last_error_warn_unix: Option<i64>, now_unix: i64) -> Option<LogAction>`（`LogAction { level, … }`）——镜像 aero-eng `verdict()` 的 pure+total 模式，单测不依赖 tracing subscriber（§6 O-AC1）。

### 1.4 日志模板（固定文案，grep 锚点）

```rust
// Err（首现 + 每 300s 重告警）：
tracing::warn!(state = 3, error = ?e,
    "audit relay scope check failed (Q0 DB error); gate fail-closed — zero claims while unverified");
// Provisioned → Disabled 转换：
tracing::warn!(state = 2, enabled, enabled_bindings,
    "audit relay scope not provisioned (provisioning revoked) — zero claims");
// 任意 → Provisioned 转换（恢复）：
tracing::info!(state = 1, enabled_bindings, "audit:event:write provisioned — claims resume");
// Unknown → Disabled（boot 即禁用，正常形态，仅一次）：
tracing::info!(state = 2, enabled, enabled_bindings,
    "audit relay scope not provisioned (relay disabled at boot); zero claims");
// 稳态 Disabled / 持续 Error 的 tick 细节：
tracing::debug!(state, enabled, enabled_bindings,
    "audit relay scope not provisioned; rows stay status 0");
```

### 1.5 relay 侧门日志富化（不改级别）

`dispatch_batch` 门命中处（base design §2.3）保持 `debug!`，加 `state` 字段（经 trait 快照方法读，§2.1）：

```rust
_ => {
    let state = self.scope_provisioner.as_ref()
        .map_or(0, |p| p.provision_snapshot().state as u8);
    tracing::debug!(state, "audit relay paused: audit:event:write not provisioned; rows stay status 0");
    return Ok(0);
}
```

`run()` 的 `warn!(?error, "audit relay claim failed…")`（relay.rs:128）与门互斥：门命中 → `Ok(0)` 不走该 warn；该 warn 只在已配给但 claim/投递失败时出现——语义不变。

## 2. Prometheus gauge（T2）：observability_gauge_samplers 模式

### 2.1 provisioner 共享快照（aero-auth，非 async，sampler 的唯一读面）

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ProvisionState { Unknown = 0, Provisioned = 1, Disabled = 2, Error = 3 }

#[derive(Debug, Clone)]
pub struct RelayProvisionSnapshot {
    pub state: ProvisionState,
    /// 仅 Error 时 Some；截断 ≤ 512 字符。
    pub last_error: Option<String>,
    /// 最近一次检查的 unix 秒（从未检查 = None）。
    pub last_checked_unix: Option<i64>,
}

pub trait RelayScopeProvisioner: Send + Sync {
    async fn audit_event_write_provisioned(&self) -> bool;
    /// 非 async 上次检查快照（gauge sampler 专用——永不触发谓词/查询）。
    /// 默认 Unknown：未插桩实现（drill 的 AlwaysProvisioned）不喂 gauge，
    /// 也不干扰生产采样。
    fn provision_snapshot(&self) -> RelayProvisionSnapshot {
        RelayProvisionSnapshot { state: ProvisionState::Unknown, last_error: None, last_checked_unix: None }
    }
}
```

`PgRelayScopeProvisioner` 持有 `Arc<SharedProvisionState>`（`AtomicU8 state` + `Mutex<Option<String>> last_error` + `AtomicI64 last_checked_unix` + 日志转换检测用 `AtomicU8 last_logged_state` + `AtomicI64 last_error_warn_unix`）。每次 `audit_event_write_provisioned()`（relay tick 与未来 assert 路径共用）更新快照 → §1.3 转换日志。**快照是 provisioner 自己的状态**；sampler 只读——严格符合 "sampler 纯观察、永不改状态"。

### 2.2 四个 gauge（`aero_server::metrics` 常量，与 INDEX_SIZE_BYTES/PG_* 同家）

| 名称 | kind | 语义 | help（register_help 原文） |
|---|---|---|---|
| `aero_audit_relay_configured` | Gauge 0/1 | boot 时 relay 配置存在（provisioner 已装） | "1 when the audit connector relay is configured at boot (scope provisioner installed); 0 otherwise. Guard label for scope_provisioned." |
| `aero_audit_relay_scope_provisioned` | Gauge 0/1 | 最近一次 Q0 检查 = provisioned | "1 when the last Q0 relay-health check reported provisioned (audit:event:write grant in effect); 0 = the fail-closed zero-claim gate is active." |
| `aero_audit_relay_scope_check_state` | Gauge 0–3 | `ProvisionState` 判别值 | "Last Q0 check outcome: 0 unknown (never checked) / 1 provisioned / 2 disabled (runtime off or zero enabled bindings) / 3 error (Q0 query failed — DB outage or schema missing)." |
| `aero_audit_relay_scope_check_age_seconds` | Gauge | 距上次检查秒数 | "Seconds since the provisioner last evaluated Q0. Growth with check_state != 1 indicates the relay poll loop stalled (no checks running)." |

`check_state == 3` 与 `check_state == 2` 的区分 = 评审要求的 "DB 故障 vs 合法禁用" 在 Prometheus 里的直接呈现（无需解析日志）。`configured` 隔离 "relay 未配"（gauge 恒 0，正常）与 "relay 配了但未配给"（告警目标）。

### 2.3 sampler 任务（`boot/metrics_tasks.rs` 新任务，30s）

完全复刻 AI DLQ / NATS backlog 任务的骨架：

```rust
// metrics_tasks.rs::spawn_all 新增参数：
//   relay_scope: Option<aero_auth::SharedRelayScopeProvisioner>
{
    for (name, help) in [ /* §2.2 四行 */ ] {
        common_metrics::global().register_help(name, common_metrics::MetricKind::Gauge, help);
    }
    let provisioner = relay_scope.clone();
    let cancel = ai_shutdown.clone();
    tracker.spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { () = cancel.cancelled() => break, _ = tick.tick() => {} }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let values = gauge_values(provisioner.as_ref().map(|p| p.provision_snapshot()), now);
            common_metrics::set_gauge(aero_server::metrics::AUDIT_RELAY_CONFIGURED, values[0]);
            common_metrics::set_gauge(aero_server::metrics::AUDIT_RELAY_SCOPE_PROVISIONED, values[1]);
            common_metrics::set_gauge(aero_server::metrics::AUDIT_RELAY_SCOPE_CHECK_STATE, values[2]);
            common_metrics::set_gauge(aero_server::metrics::AUDIT_RELAY_SCOPE_CHECK_AGE_SECONDS, values[3]);
        }
    });
}
```

**sampler 永不查询 DB**（读共享快照而非调谓词）——快照设计正是把可观测性移出 DB 路径：新增的 30s 任务不产生任何 SQL，`dispatch_batch` 之外无第二查询源（§3 的成本核算因此只含 relay tick 的 Q0）。`gauge_values(snapshot: Option<RelayProvisionSnapshot>, now_unix: i64) -> [f64; 4]` 为纯函数（映射：None → `[0,0,0,0]`；Some → `[1, state==Provisioned, state, now - last_checked]`），单测直接断言（§6 O-AC2）。

### 2.4 接线（增量，不破坏 base design D5 单构造点）

- `boot/metrics_tasks.rs::spawn_all` 签名追加 `relay_scope: Option<aero_auth::SharedRelayScopeProvisioner>`。
- `main.rs` 在 `spawn_metrics_tasks(…)` 调用追加 `services.relay_scope_provisioner.clone()`。**部分 move 合法性**：`services.auth` 于 :185 move 进 state，其他字段仍可读（`services.ai_service` 在 :240 的既有读取是先例）；`spawn_metrics_tasks` 调用点在 :270，同一函数作用域。
- provisioner 仍只在 `boot/services.rs` 构造一次（D5）——relay 臂、sampler、AuthService 三者共享同一 `Arc`，无第二构造点。

### 2.5 告警规则与多实例语义

```
# 未配给零 claim（核心信号）：配置存在但门关闭 ≥ 2 分钟
aero_audit_relay_configured == 1 and aero_audit_relay_scope_provisioned == 0 → warn
# Q0 DB 故障 / schema 缺失（区别于合法禁用）
aero_audit_relay_scope_check_state == 3 → warn
# relay 轮询循环停滞（连检查都不跑了——快照变旧）
aero_audit_relay_scope_check_age_seconds > 120 and aero_audit_relay_configured == 1 → warn
```

多实例：每实例 sampler 写同一 series（与既有 DB_POOL 类 gauge 同语义）。配给真值在 PG 层，各实例应一致；瞬时分歧 ≤ 一个检查周期（5s 默认），可接受——文档注明即可。

## 3. Q0 免缓存裁决（T3）

**裁决：每 `dispatch_batch` 单例 Q0 查询不做缓存**。理由三层：

### 3.1 成本核算（有界且可忽略）

- **查询形状**：`snaplink_commercial_runtime` 单行 PK 查找（singleton BOOLEAN PRIMARY KEY，0235:8-11，一次索引探测）+ `snaplink_commercial_bindings WHERE enabled` 的 count。bindings 是**控制面表**（每商业租户一行，PK workspace_id + 三个 UNIQUE，DDL 无 `enabled` 索引）——量级为个位到低百行，全表扫 sub-ms；**不需要**为此加索引（加索引反而破坏 0235 零迁移约束且对小表更慢）。
- **频次**：`dispatch_batch` 每 `poll_interval` 一次（默认 5s，min 1s，max 300s——`config.rs:88`，运维可调），每实例 ≈ 17k 次/天（默认）。`shutdown_drain` 的突发循环有界（shutdown_drain 超时内，仅关停期）。`assert_audit_scope_provisioned` 现状零调用方；未来机器端点若每请求一次，其成本级别 = 既有 `PatRepo` 每请求校验 PAT 的查询（pat.rs:46）——同量级、有先例。
- **对比基准**：`PatRepo`/`BotRepo` 的 verifier 每请求查库（无缓存），本查询频率远低于它们。

### 3.2 缓存语义分析（每条都指向免缓存）

| 缓存方案 | 后果 |
|---|---|
| 正结果缓存（true → TTL T） | **DB 故障期间缓存 true 继续放行 claim**——直接违反 R1 "任何异常 → false" 与本 campaign 的核心 fail-closed 属性。绝对不可接受 |
| 负结果缓存（false → TTL T） | 恢复收敛延迟 ≤ T（违反 D3 ≤1-tick 收敛）；且 T 内 DB 故障与禁用不可区分（恰是本任务 T1 要消除的盲区） |
| error-aware 缓存（仅 Ok 时缓存，Err 失效重查） | 故障时（门存在的唯一理由）无缓存收益；正常时 1 次/5s 的查询省无可省；多一个陈旧域 + 失效逻辑 = 纯负收益 |
| 任意 TTL ≥ poll_interval | 与无缓存等价（每次 tick 前已过期）；TTL < poll_interval 无意义 |

### 3.3 逃生口

若未来性能剖析真出现 Q0 争用（本查询形状下不会），**逃生口是调大 `AERO_AUDIT_POLL_INTERVAL_SECS`**，不是引入缓存——频次是唯一可变维度，且它同时约束门收敛速度与日志频率。裁决写入 §2.3 sampler 零查询 + §3.1 成本核算，无代码位。

## 4. `aero-audit-priority-drill` 覆盖接线（T4）

### 4.1 现状（评审事实核实）

- bin 已被 CI **间接**执行：`test-integration.sh` :455-481 跑 `aero-cli audit-provision-check --priority`（aero-eng），`run_priority`（audit_provision.rs）第三步 spawn `cargo run … --bin aero-audit-priority-drill`（`AERO_PRIORITY_DRILL_BIN` 可覆盖）并透传退出码 0/1/2。
- 但 **bin 名不出现在任何脚本文本**（评审 grep 落空 = 覆盖不可审计）；直接跑 bin = 手动-only；且 priority 节无 CLI 存在性门（CLI 未落地时整节 FAIL，不像 T-11 节有 `B5_HELP_OUT` grep :368-379 先例）。

### 4.2 test-integration.sh 接线规格（moderation-priority-drill 节，0239 文件门内）

1. **显式构建 + 覆盖注入**（使 bin 名文本可 grep、消除内层隐式 `cargo run` 的二次编译）：

```bash
# B5-4 observability: build the drill bin explicitly and hand it to the
# aero-eng CLI via the AERO_PRIORITY_DRILL_BIN override (audit_provision.rs
# run_priority step 3) so THIS script executes the bin textually — its name
# must be grep-able here, not spawned invisibly by an inner cargo run.
cargo build --quiet -p aero-audit-connector --bin aero-audit-priority-drill
export AERO_PRIORITY_DRILL_BIN="$PWD/target/debug/aero-audit-priority-drill"
```

2. **CLI 存在性门 + 直跑 fallback**（复用 T-11 节 :368-379 的 `B5_HELP_OUT` 检测模式；两分支共享既有 0/2/else 判定骨架）：

```bash
B5_HELP_OUT="$(cargo run -p aero-cli -- help 2>&1 || true)"
if grep -q "audit-provision-check" <<<"$B5_HELP_OUT"; then
    # CLI leg（base check + verdict 行 + 经 AERO_PRIORITY_DRILL_BIN 跑 drill）——现逻辑不动
    DATABASE_URL="$PRIORITY_DRILL_URL" AERO__DATABASE__URL="$PRIORITY_DRILL_URL" \
        cargo run --quiet -p aero-cli -- audit-provision-check --priority >"$PRIORITY_DRILL_LOG" 2>&1
    PRIORITY_DRILL_RC=$?
else
    # Direct-bin leg：覆盖不依赖 aero-eng CLI 落地；退出码契约一致
    DATABASE_URL="$PRIORITY_DRILL_URL" \
        "$AERO_PRIORITY_DRILL_BIN" >"$PRIORITY_DRILL_LOG" 2>&1
    PRIORITY_DRILL_RC=$?
fi
```

判定骨架（两分支共用，现存代码不变）：`0` → CLI leg 需 `grep -q "priority: landed"`、direct leg 直接 PASS（bin 内部断言即裁决，PASS 行 `drill: moderation-in-first-batch: PASS` / `drill: drain-501: PASS` / `drill: parity-501: PASS` 保持可见）；`2` → SKIP（0239/priority/class 未落地）；其余 → FAIL + 日志上屏 + exit 1。`b5_check "moderation-priority-drill"` 行保持。

### 4.3 退出码契约（bin 文档注释 + 本设计双写）

| 码 | 含义 | 脚本判定 |
|---|---|---|
| 0 | 全部断言 PASS（首批含 moderation 行 + 501 全 drain + event_id 集合平价） | PASS |
| 2 | SKIP：0239 表或 priority/class 列缺失（B5-1/B5-3 未落地窗口） | SKIP |
| 1 | bail：排序/批量契约被违（0239 落地但 B5-3 未落地 = 红 FAIL） | FAIL |
| 其他 | 异常退出/超时（120s 上限在 run_priority 侧） | FAIL |

### 4.4 手动门禁命令（文档化，drill 开发/预 CI 用）

```bash
# 一次性库（drill 会 TRUNCATE audit_governance_outbox——禁在共享 dev 库跑）
make migrate-smoke  # 或：DATABASE_URL=<一次性库> cargo run --bin aero-cli -- migrate
DATABASE_URL=postgres://…/aero_priority_drill_manual \
    cargo run -p aero-audit-connector --bin aero-audit-priority-drill
echo $?   # 0 = PASS / 1 = FAIL / 2 = SKIP（stub sink 自启动，无外部依赖）
```

### 4.5 与构造点注入的关系

drill 构造点（priority-drill.rs:203）按 base design 注入 `AlwaysProvisioned`（恒 true）——**本设计零改动**；其 `provision_snapshot()` 走 trait 默认 `Unknown`，不影响生产 gauge。relay.rs:374 内部测试直呼 `deliver_claim` 绕过门，无需注入（base design 修正点，本设计沿用）。

## 5. API 增量（per crate，相对 base design §2）

| crate | 增量 | 兼容性 |
|---|---|---|
| `aero-storage/src/relay_scope.rs` | `q0_provisioned() -> Result<(bool, i64)>`（base 的 `-> Result<bool>` 改签名——新符号，零兼容成本） | 无既有调用方 |
| `aero-auth/src/relay_scope.rs` | + `ProvisionState`（repr(u8)，0-3）、`RelayProvisionSnapshot`、trait `provision_snapshot()`（默认 Unknown）、`PgRelayScopeProvisioner { repo, shared: Arc<SharedProvisionState> }`、纯函数 `next_log`（§1.3） | trait 默认方法 = 既有/未来 impl（AlwaysProvisioned、sibling ComposedProvisioner）零改动 |
| `aero-server/src/metrics.rs` | + 4 个名字常量（§2.2） | 纯新增 |
| `aero-server/src/bin/boot/metrics_tasks.rs` | `spawn_all` + 参数 `relay_scope`；+ 30s sampler 任务（§2.3） | 调用点仅 main.rs 一处 |
| `aero-server/src/bin/main.rs` | `spawn_metrics_tasks(…, services.relay_scope_provisioner.clone())` | 部分 move 合法（:185 < :270） |
| `aero-audit-connector/src/relay.rs` | 门命中 `debug!` 加 `state` 字段（§1.5） | 级别/行为不变 |
| `scripts/test-integration.sh` | priority 节接线（§4.2） | 现存判定骨架不动 |
| 迁移 / env / 依赖 | **零新增** | 0235 事实源不变；aero-auth 既有 tracing（Cargo.toml:25） |

## 6. 验收映射（每项有命令/断言）

| 验收 | 落点 | 断言/命令 |
|---|---|---|
| **O-AC1** 日志转换状态机 | `aero-auth/src/relay_scope.rs` 单测（纯函数 `next_log`，无 tracing subscriber） | 全转换矩阵：Err 首现 → warn；Err 持续 300s 内不再 warn、超 300s 重 warn；Provisioned→Disabled → warn；boot Disabled → info；恢复 Provisioned → info；稳态静默/debug |
| **O-AC1b** 可区分性 | 同上（`LogAction` 断言） | Err 行带 `error=` 字段；Disabled 行带 `enabled`/`enabled_bindings` 字段；文案 "check failed" vs "not provisioned" |
| **O-AC2** gauge 映射 | `aero-server` 单测 `gauge_values`（纯函数）+ `metrics_tasks` sampler 骨架测试 | `None` → `[0,0,0,0]`；`Provisioned` → `[1,1,1,age]`；`Disabled` → `[1,0,2,age]`；`Error` → `[1,0,3,age]`；age = now − last_checked_unix；渲染含 `aero_audit_relay_scope_provisioned` |
| **O-AC3** 每次 dispatch_batch 单例 Q0 | `tests/state_machine.rs` AC1a 用例扩展：计数 provisioner | `dispatch_batch()` 恰好调用谓词 1 次（计数断言）；零 claim 行保持 status 0（既有断言） |
| **O-AC4** drill 接线 | `scripts/test-integration.sh` | priority 节含文本 `aero-audit-priority-drill`（可 grep）；CLI 存在 → CLI leg 跑通（0 PASS / 2 SKIP）；CLI 缺失 → direct leg 同样判定；`b5_check "moderation-priority-drill"` 保持 |
| **O-AC5** 手动门禁 | 本设计 §4.4 + bin doc-comment | 命令文档化；退出码 0/1/2 契约双写 |
| 门禁 | 全量 | `cargo check --workspace` · `cargo test --workspace --lib` · `cargo test -p aero-auth`（O-AC1）· `cargo test -p aero-audit-connector`（O-AC3，integration target）· `cargo clippy --workspace --all-targets`（无新警告）· `scripts/{truth-check,file-size-check,web-check}.sh` |

## 7. 决策点与风险

- **D-O1（快照经 trait 方法，非独立通道）**：`provision_snapshot()` 放 trait（默认 Unknown）——保持 base design D6 的"单 seam 族"：sibling 的 `ComposedProvisioner` 实现时委托 q0 子实现的快照即可；不新增 Services/AppState 第二通道。代价：trait 面 +1 默认方法（additive）。
- **D-O2（gauge 名字落 `aero_server::metrics` 而非 `aero_common::metrics::names`）**：四个 gauge 只有 sampler（aero-server）写——与 INDEX_SIZE_BYTES/PG_* 同家、aero-common 零改动。若未来 provisioner 自身要 inc 计数器（aero-auth 侧），名字届时迁 aero-common（aero-auth → aero-common 无环，可行）。**本设计不建错误计数器**：`check_state == 3` + `check_age_seconds` 已覆盖"是否在错、错了多久"；错误速率告警留作未来扩展。
- **D-O3（300s 重告警节流）**：Err 首现立即 warn，持续 Err 每 300s 重告警一次——上限 ≈288 行/天/实例，且转换级即时性保留。300 与 poll_interval 上限（300s）同量级：极端配置下重告警间隔 ≥ 检查间隔，不丢转换。
- **D-O4（drill 双轨而非去重）**：保留 CLI leg（它额外产出 base check + verdict 行，是 `audit-provision-check` 门禁本体），直跑 leg 只作 CLI 缺失 fallback——**不重复跑 drill**（每跑一次 = 一次性库 migrate + 501 行 drain）。`AERO_PRIORITY_DRILL_BIN` 覆盖使单次执行文本可审计。
- **残余风险**：① 多实例 gauge 同 series 互覆（既有模式同款，配给真值在 PG 层一致）；② 稳态 Disabled 只有 debug——运维若想日常可见需开 debug 或靠 gauge（gauge 是主信号，日志是事故定位）；③ enum gauge 值 0-3 的语义靠 help 文本固化（§2.2），变更须同步 help。base design 的其余评审修正（构造面 6→7、0235 删行 Err 用例、scope-agnostic 声明、v1 relay 边界行）属 base 文档，本设计不重述。

## 8. 落地排序（叠加在 base design §5 之上）

1. **aero-storage**：`q0_provisioned` 改返回事实对 + db_test（含 0235 删行 → `Err` 用例，呼应评审修正）。
2. **aero-auth**：`ProvisionState`/`RelayProvisionSnapshot`/trait 默认方法/`SharedProvisionState`/`next_log` 纯函数 + O-AC1/O-AC1b 单测 → `cargo test -p aero-auth`。
3. **aero-server**：metrics 常量 + `metrics_tasks` 参数与 sampler + main.rs 传参 + `gauge_values` 单测（O-AC2）→ `cargo check --workspace`。
4. **connector**：门 `debug!` 富化 + AC1a 计数扩展（O-AC3）→ `cargo test -p aero-audit-connector`。
5. **脚本**：test-integration.sh priority 节接线（O-AC4）→ 跑 `scripts/test-integration.sh` 确认三 drill 节 PASS/SKIP 判定不变。
6. **门禁**：O-AC1..5 全绿 + 全量 test/clippy/truth-check/file-size-check/web-check。
