# Aero IM — 架构分析报告（基于 5 个零覆盖系统性缺口）

> **分析范围**：全量代码库 16 crate / 157 迁移 / ~46K Rust + ~6K Web SPA
> **分析依据**：`docs/requirements/2026-07-11-5-zero-coverage-gaps.md`
> **分析视角**：架构评估 → 扩展方向 → 接口设计 → 技术选型 → 实施路线图

---

## 一、架构评估

### 1.1 当前架构的核心优势

文档所揭示的 5 个缺口，反而说明架构在**单一维度内**做得相当扎实：

| 架构特质 | 证据 |
|---------|------|
| **强隔离的故障域** | 6+ 独立 fail-open 路径，每个组件自保不连锁崩溃 |
| **预算隔离的 AI 管线** | per-workspace + global 两级 budget，防单个租户饿死全局 |
| **清晰的事件溯源边界** | `im.room.*` vs `live.stream.*` 分 subject 空间，各维护独立 seq |
| **可拆卸的后端抽象** | `BlobStore` trait 支持 LocalFs/S3 切换；`AiBackend` 有完整 degrade 实现 |
| **无状态水平扩展** | NATS 扇出 + Redis 集群状态，每进程 Hub 只做 bounded mpsc 扇出 |

**架构哲学**：系统在「单一故障不扩散」上做得很好——每个组件都有自己的故障边界和降级策略。这是典型的微服务意识在单体 crate 架构中的体现。

### 1.2 关键设计决策的合理性评估

| 决策 | 合理 | 需重新审视 |
|------|------|-----------|
| Per-subject NATS seq | ✅ 单一 subject 内强有序 | ❌ 跨 subject 因果缺失（方向一） |
| AI budget defer（不耗 retry） | ✅ budget 抖动不杀 job | ❌ 活锁风险（方向二） |
| 各组件独立 fail-open | ✅ 自保不崩溃 | ❌ 级联降级无聚合视图（方向三） |
| `sqlx::migrate!` 编译期嵌入 | ✅ 一致性保证 | ❌ 零停机部署阻塞（方向四） |
| Passive blob GC（仅 GDPR 触发） | ✅ 简单可靠 | ❌ 累积数据债（方向五） |

### 1.3 架构债务与技术债

按严重度分类：

#### P1 架构债务

1. **因果隐含假设（方向一）**：系统隐含地假设「所有事件最终由一个时钟排序」，但分布式现实中这个假设不成立。这是最危险的债务——它不在代码中表现为 bug，而是表现为**偶发的、不可复现的用户可见不一致**。

2. **监控盲区（方向三）**：6 个独立 fail-open 路径没有任何聚合告警。这是运维层面的债务——系统可以在「每个组件都降级」的状态下运行而不触发任何告警，运营团队直到用户投诉才知道。

3. **Budget 活锁（方向二）**：defer 机制的设计虽然避免了 budget 抖动杀 job，但引入了**活锁**——比死锁更难诊断，因为负载降低后自行恢复，问题在事后日志中几乎不可见。

#### P2 架构债务

4. **迁移不可逆（方向四）**：157 个 up-only 迁移堵死了零停机发布路径。这不是立即爆炸的问题，而是一个「当需要回滚时已经太晚」的债务。

5. **数据完整性无巡视（方向五）**：幽灵数据累积是慢性的。单个 blob 断裂不可怕，但年复一年无校验的积累在监管审计时会变成 blocker。

---

## 二、扩展方向

根据文档的 5 个方向和我的分析，我提出 5 个扩展方向（重新组织了优先级和依赖关系）：

### 方向 A（P0 · 立即）：AI Budget Defer 逃生阀

> 这是 5 个方向中**实现成本最低、商业风险最直接**的。审核/嵌入/摘要停滞不执行直接违反合规预期。

**为什么是 P0 不是 P1**：持续过载下的 job 无限 defer 等价于「AI 审核功能静默关闭」。在有法规要求必须审核内容的应用场景中，这是合规风险。

**架构变更**：

```
现状:
  Worker claim → budget 不足? → defer (scheduled_at = now + 60s) → 下轮再 claim → ...

目标:
  Worker claim → budget 不足? → defer + defer_count++
    → defer_count >= MAX_DEFERS(10)?
        → 是: 强制运行 (override budget) + 告警
        → 否: defer + 指数退避 (1x, 2x, 4x... 窗口)
```

**需修改的模块**：
- `aero-ai/src/worker/mod.rs`：claim 循环中插入 defer_count 检查和强制运行路径
- `aero-ai/src/ai_job.rs`：defer 方法更新 `defer_count`，加 `force_run` 方法
- `migrations/`：`ALTER TABLE ai_jobs ADD COLUMN defer_count INTEGER DEFAULT 0`

**选项权衡**：

| 选项 | 优点 | 缺点 |
|------|------|------|
| A. 强制运行（超 budget） | 简单，保证 job 最终执行 | burst 瞬间可能耗尽预算，其他 workspace 受冲击 |
| B. 送入 DLQ + 人工介入 | 安全，运维可见 | 需要人工恢复，延迟更高 |
| C. 降级到最便宜的模型 | 保底运行不花 full cost | 复杂度高，需 `AiBackend` 扩展 |
| **推荐：A + B 混合** | 前 10 次 defer 后强制运行 + 第 20 次强制 DLQ | 中等复杂度，兼顾安全与最终执行 |

### 方向 B（P1 · 新能力）：System Health Aggregator 与降级可观测性

> 基础监控升级，为方向 A 的逃生阀告警提供接收通道，也为所有 fail-open 路径提供统一视图。

**核心挑战**：不引入新的中心化依赖（不把监控本身变成 SPOF），同时提供聚合视图。

**建议架构**：

```
┌─────────────────────────────────────────────────────────┐
│                 HealthAggregator (每进程)                  │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐              │
│  │ RateLimit │  │ AI       │  │ Push Bot │  ...         │
│  │ fail-open │  │ degrade  │  │ skip     │              │
│  │ counter   │  │ counter  │  │ counter  │              │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘              │
│       │              │              │                    │
│       ▼              ▼              ▼                    │
│  ┌──────────────────────────────────────────────────┐   │
│  │           Aggregate Health State                  │   │
│  │  mode: Normal │ Degraded(1) │ Severe(2+)         │   │
│  └──────────────────────┬───────────────────────────┘   │
│                         │                                │
│                         ▼                                │
│              Prometheus gauge (per-process)              │
│         aero_system_health_mode{component="..."}         │
└─────────────────────────────────────────────────────────┘
```

**设计原则**：
1. **去中心化**：每个进程独立的 `HealthAggregator`，通过 Prometheus 汇集，不引入中心化健康存储
2. **有界状态**：只跟踪 `fail_open` 的计数器，不跟踪每个请求的健康状态——开销 O(组件数) 而非 O(请求数)
3. **冷却机制**：fail-open 后的冷却窗口（5 秒），避免间歇性抖动导致状态频繁切换

**修改范围**：
- 新增 `aero-health` crate（或放入 `aero-common`）：`HealthAggregator` struct + `HealthComponent` trait
- 6+ 现有 fail-open 路径：嵌入 `health.on_component_fail_open(component_name)`
- `metrics.rs`：注册 `aero_system_health_mode` gauge
- 可选的 WS 推送：`{"type":"degraded","service":"ai"}`

### 方向 C（P1 · 基础设施）：因果一致性框架

> 这是**技术最复杂、影响面最广**的方向。需要谨慎分阶段实施。

**核心挑战**：
1. HLC 引入需要所有节点改造时间戳生成——这是**横切变更**
2. 跨 subject 因果跟踪需要 NATS 消息携带依赖元数据——增加 per-message 开销
3. 客户端因果排序引擎增加 Web SPA 复杂度——JS 需维护 DAG

**建议分期实施**：

**Phase 1（最小可行）**：仅改造转发（Forward）和通知（Notify）路径
- `forward.rs` 在发布到目标 subject 前等待源 subject ack
- 引入 `message.causal_order` JSONB 字段，记录 `(source_subject, source_seq)` 元组
- 不引入 HLC，不改造客户端排序

**Phase 2（完整方案）**：
- 引入 `hybrid-clock` crate 实现 HLC
- 所有 `RoomEvent` 和 `StreamEvent` 携带 HLC 时间戳
- 客户端 `ws.js` 从 ULID 排序迁移到 `(hlc_ts, node_id)` 排序
- 因果暂存区（bounded stash，1000/room）

**设计决策权衡**：

| 决策 | 选项 A：HLC | 选项 B：矢量时钟 | 选项 C：仅等待机制 |
|------|------------|----------------|------------------|
| 复杂度 | 中 | 高（N 节点→矢量长度增长） | 低 |
| 因果保证 | 强（happens-before 偏序） | 全序 | 仅显式依赖 |
| 带宽开销 | 小（~12 bytes/msg） | O(N) | O(依赖数) |
| 时钟偏移容错 | 是 | 不依赖时钟 | 否 |
| **推荐** | ✅ **用于 Phase 2** | ❌ 不适合此规模 | ✅ **用于 Phase 1** |

### 方向 D（P2 · 工具链）：零停机迁移框架

> 这是工程治理层面的投入，短期无用户可见收益，但长期阻塞企业 SLA。

**不推荐重写迁移体系**：157 个迁移不可能全部写出 `down.sql`。建议「冻结 + 新策略」：

```
冻结:
  - 现有 157 个迁移标记为 "historical"（记录基线快照）
  - 编写一个 "squash migration"——建一个基线 schema dump
  - 启动时跳过 historical 迁移（如果 baseline 存在）

新策略 (v158+):
  1. 每个迁移必须带 down.sql（至少对 destructive 操作）
  2. 引入 Expander → Contractor 两阶段模式
     - Expander (up)：添加新列/表，旧版兼容
     - Contractor (down)：废弃旧列，仅在所有实例升级后执行
  3. 后台迁移模式：迁移运行时 /ready 返回 503，/live 返回 200
```

**关键文件修改**：
- `aero-storage/src/db.rs`：迁移执行逻辑支持 baseline detection、后台迁移、schema 版本兼容
- `routes/routes.rs`：新增 `GET /api/debug/schema-version`
- `config.example.toml`：新增 `AERO_DB_SCHEMA_VERSION` 配置项

### 方向 E（P2 · 数据治理）：Data Scrubber 框架

> 最低业务价值但最高运维价值——数据债的利息是按月累积的。

**建议不新建 Worker**：复用已有 timer 模式（`blob_gc_drain` timer 的相邻模式），减少新增常驻进程的数量。

```rust
// 不新建 bin/boot/ 条目，而是扩展已有 timer：
// bin/boot/timers.rs 中新增一个 timer:

tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(86400)); // 24h
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(e) = scrubber::run_once(&pool, &blob_store, &metrics).await {
            tracing::warn!(error = %e, "scrubber iteration failed");
        }
    }
});
```

**建议分模式**：

| 模式 | 干的事 | 间隔 | 默认 |
|------|--------|------|------|
| `report-only` | 检查 + Prometheus 指标 + 日志 | 24h | ✅ |
| `fix-orphan-blob` | report + 自动 enqueue orphan 到 GC | 24h | ❌ (opt-in) |
| `fix-orphan-ref` | report + 清理孤立 reaction/notification | 24h | ❌ (opt-in) |

---

## 三、接口设计建议

### 3.1 HealthAggregator 的接口设计

```rust
// aero-health/src/lib.rs

/// 健康组件 trait——任何降级组件实现此 trait
#[async_trait]
pub trait HealthComponent: Send + Sync {
    /// 组件是否为降级状态
    fn is_degraded(&self) -> bool;
    /// 组件名称（用于 Prometheus label）
    fn name(&self) -> &'static str;
    /// 降级原因（可选，用于告警上下文）
    fn degradation_reason(&self) -> Option<String>;
}

/// 聚合健康状态
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HealthMode {
    /// 所有组件正常
    Normal = 0,
    /// 1 个组件降级
    Degraded = 1,
    /// 2+ 组件降级
    Severe = 2,
}

/// 进程级健康聚合器
pub struct HealthAggregator {
    components: Vec<Box<dyn HealthComponent>>,
    // 冷却机制防止抖动
    cooldown: Duration,
    last_transition: HashMap<&'static str, Instant>,
}
```

**设计原则**：
1. **Pull 模式**：HealthAggregator 定期检查组件状态，而非组件主动 push——减少耦合
2. **注入而非继承**：组件不需要改 trait 实现，只需在 fail-open 调用 `aggregator.on_fail_open(name)`
3. **Prometheus native**：状态直接暴露为 gauge，不经过额外 HTTP 端点

### 3.2 因果一致性接口

```rust
// aero-bus/src/causality.rs

/// 因果依赖——描述一个事件在因果上依赖于另一个事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CausalDependency {
    pub subject: String,      // "im.room.{id}"
    pub seq: u64,             // 该 subject 上的 seq
    pub hlc: Option<HlcTimestamp>, // Phase 2 用
}

/// 携带因果元数据的 NATS 消息封装
pub struct CausalMessage<T> {
    pub payload: T,
    pub depends_on: Vec<CausalDependency>, // 前因列表
    pub hlc: Option<HlcTimestamp>,
}

/// 因果暂存区——等待前因到达
pub struct CausalStash<T> {
    max_capacity: usize,       // 1000/room
    stash: Vec<StashedMessage<T>>,
    timeout: Duration,         // 5s 超时强制 flush
}
```

### 3.3 Scrubber 接口

```rust
// aero-scrubber/src/lib.rs (或放入 aero-storage/src/scrubber.rs)

/// Scrubber 配置
#[derive(Clone)]
pub struct ScrubberConfig {
    pub batch_size: usize,           // 100
    pub throttle_per_sec: u32,       // 50
    pub fix_mode: FixMode,
    pub report_only: bool,
}

/// Scrubber pass——一个扫描+校验的完整回合
#[derive(Serialize)]
pub struct ScrubReport {
    pub checked: HashMap<ResourceKind, usize>,
    pub failed: HashMap<ResourceKind, Vec<ScrubFailure>>,
    pub duration: Duration,
    pub resources_skipped: usize,
}

#[derive(Serialize)]
pub struct ScrubFailure {
    pub resource_id: String,
    pub reason: ScrubFailureReason,
    pub fix_applied: bool,
}
```

### 3.4 迁移框架接口

```rust
// aero-storage/src/migration.rs

/// 两阶段迁移
pub trait TwoPhaseMigration: MigrationTrait {
    /// Expander 阶段：添加新结构，旧版兼容
    async fn expand(&self, manager: &SchemaManager) -> Result<(), DbErr>;
    /// Contractor 阶段：废弃旧结构，仅在确认全部实例升级后
    async fn contract(&self, manager: &SchemaManager) -> Result<(), DbErr>;
    /// Down.expand（回滚 expand 阶段）
    async fn contract_down(&self, manager: &SchemaManager) -> Result<(), DbErr>;
    /// Down.contract（回滚 contract 阶段）
    async fn expand_down(&self, manager: &SchemaManager) -> Result<(), DbErr>;
}
```

---

## 四、技术选型

### 4.1 是否引入新依赖

| 候选 | 场景 | 建议 | 理由 |
|------|------|------|------|
| `hybrid-clock` crate | 因果一致性 Phase 2 | **自建**（< 200 行） | 需求极简单：`(wall_time, logical, node_id)` 三元组，不值得引入新依赖 |
| `axum-extra` | HealthAggregator 端点 | **不引入** | 已有 `routes.rs` 体系，新增一个 Debug 路由即可 |
| `opentelemetry` 深度集成 | 降级监控 | **部分引入**（已有 OTLP 集成） | 已有 `aero-common/src/telemetry.rs`，扩展 gauge 注册即可 |
| DI 容器（如 `cakesmore`） | 模块间解耦 | **不引入** | Rust 的 trait + constructor injection 已经足够，DI 容器在 Rust 生态中属于异端 |
| `tower` 中间件体系 | 降级检测 | **利用已有** | 已有 `tower_http` 依赖，健康检查中间件可复用 |

**核心原则**：这 5 个方向**都不需要引入新的第三方 crate**。所有功能可基于现有 tokio + sqlx + Prometheus 生态实现。这是好的——表明缺口是设计缺陷而非技术能力不足。

### 4.2 自建 vs 复用评估

| 功能 | 自建成本 | 第三方替代 | 推荐 |
|------|---------|-----------|------|
| HLC | 低（~150 行） | `hybrid-clock` crate | **自建**——需求太简单 |
| Health aggregator | 低（~200 行） | Health check 框架 | **自建**——特定于 fail-open 语义 |
| Schema migration 框架 | 中（~500 行） | `sqlx::migrate` 已用 | **扩展现有**——不替换 sqlx，加两阶段能力 |
| Scrubber | 中（~400 行） | 通常自建 | **自建**——高度特定于业务 schema |
| 因果暂存区 | 中（~300 行） | 不大可能有现成 | **自建** |

### 4.3 Rust 生态约束

这些方向的共同限制来自 Rust 异步生态：
- **无运行时反射**：`HealthAggregator` 不能自动发现所有 `HealthComponent` 实现——需**显式注册**
- **编译时检查 vs 运行时弹性**：fail-open 路径天然是运行时决策，Rust 的类型系统无法编译期保证「所有降级路径都有监控」
- **ORM 限制**：`sqlx` 的编译期 SQL 检查对动态 migration（如分区工具）不友好

---

## 五、实施路线图

### 5.1 优先级矩阵

```
                   高
                   ↑
          业务影响  │  方向A(Defer逃生阀)    方向C(因果一致性)
                   │  P0                    P1
                   │  
                   │  方向B(HealthAgg)      
                   │  P1                    
                   │  
                   │  方向D(零停机迁移)     方向E(Scrubber)
                   │  P2                    P2
                   │
                   └──────────────────────────────→
                      低         实现成本        高
```

### 5.2 分阶段实施

#### Sprint 1-2（2 周）：快速修补 — 方向 A + 方向 B 基础

**里程碑**：AI 管线活锁解除 + 降级可见性基础

| 任务 | 预估 | 涉及文件 |
|------|------|---------|
| `ai_jobs` 加 `defer_count` 列 + 迁移 | 1d | `migrations/`, `ai_job.rs` |
| Worker defer 循环插入计数检查 + 逃生阀 | 2d | `worker/mod.rs` |
| Worker 强制运行路径 + 告警日志 | 1d | `worker/mod.rs` |
| HealthAggregator 核心 struct + 注册 | 2d | 新文件 `aero-common/src/health.rs` |
| Rate limit / AI / Push / Moderation 嵌入 | 2d | 逐个文件 |
| Prometheus gauge + 简单 Grafana 面板 | 1d | `metrics.rs`, 部署仓库 |
| WS 降级信号（可选） | 1d | `hub.rs`, `ws.js` |

**风险**：逃生阀强制运行可能在某些极端 burst 下让高峰 budget 异常——需监控 `budget_override_total` 并在超过阈值时自动告警。

#### Sprint 3-4（3 周）：深度能力 — 方向 C Phase 1 + 方向 E 基础

**里程碑**：转发路径因果保障 + Scrubber report-only

| 任务 | 预估 | 涉及文件 |
|------|------|---------|
| Forward 等待机制（源 subject ack 后发目标） | 2d | `forward.rs`, `bus.rs` |
| `causal_dependency` 元组字段 + 序列化 | 1d | `event.rs` |
| Scrubber report-only 模式 + blob 校验 | 3d | 新文件 `storage/src/scrubber/` |
| Orphan message/reaction/notification 检测 | 2d | 同上 |
| Prometheus scrubber 指标 | 1d | `metrics.rs` |

**风险**：
- Scrubber 在大表上的 SELECT 可能影响主路径性能——必须加 `throttle` 和 `batch_size` 限制
- Forward 等待机制增加延迟——需设定超时（默认 5 秒超时后直接发，不阻塞）

#### Sprint 5-6（3 周）：基础设施 — 方向 C Phase 2 + 方向 D 规划

**里程碑**：HLC 引入 + 迁移框架方案落地

| 任务 | 预估 | 涉及文件 |
|------|------|---------|
| HLC 实现 + 单元测试 | 2d | `common/src/hlc.rs` |
| RoomEvent/StreamEvent 加 HLC 戳 | 1d | `event.rs` |
| WS 客户端排序迁移到 HLC | 3d | `ws.js`, `app.js` |
| 因果暂存区实现 | 2d | `bus/src/stash.rs` |
| 迁移体系方案设计 + 文档 | 2d | `docs/runbooks/` |
| 157 个历史迁移基线快照 | 1d | `db.rs` |
| Expander/Contractor 框架原型 | 2d | `aero-storage/src/migration.rs` |

**风险**：
- HLC 改造 影响面极大（所有事件生产者 + 消费者 + 客户端）——需 feature flag 灰度
- WS 客户端从 ULID 到 HLC 排序是破坏性变更——旧客户端不兼容，需要版本协商

#### Sprint 7-8（2 周）：收尾与韧性验证

**里程碑**：所有方向可上线 + Chaos Engineering 验证

| 任务 | 预估 | 涉及文件 |
|------|------|---------|
| Scrubber fix-orphan 模式 | 2d | `scrubber/` |
| 降级 Chaos Engineering 测试 | 2d | 测试脚本 |
| 运维 runbook 更新 | 1d | `docs/runbooks/` |
| 端到端因果一致性测试 | 2d | 集成测试 |
| 迁移 down.sql 试点（选 3 个关键迁移写 down） | 1d | `migrations/` |

### 5.3 依赖关系图

```
Sprint 1-2 (方向A + 方向B基础)
  │
  ├──→ Sprint 3-4 (方向C Phase1 + 方向E基础)
  │     │
  │     └──→ Sprint 5-6 (方向C Phase2 + 方向D)
  │           │
  │           └──→ Sprint 7-8 (收尾 + Chaos)
  │
  └──→ Sprint 7-8 (方向A 的 dashboard 完善——可并行)
```

- 方向 A 和方向 B 可以完全并行（不同模块）
- 方向 C Phase 1 依赖方向 B 的 HealthAggregator（用于因果暂存区状态的监控）
- 方向 E 可以随时插入（不依赖其他方向）
- 方向 D 的 baseline 快照需要冻住 157 个迁移，建议在其他方向迁移之前做

### 5.4 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| HLC 改造影响面过大，持续 2+ 个 Sprint | 中 | 高 | Phase 1 不做 HLC；Phase 2 用 feature flag 灰度 |
| Scrubber 在生产环境造成 I/O 竞争 | 中 | 中 | 默认 50/s 节流；report-only 先跑 2 周 |
| Defer 逃生阀误触发（正常 burst 被标记为活锁）| 低 | 低 | MAX_DEFERS 从 10 开始，监控后调整 |
| 迁移框架改造需要修改 157 个现存迁移 | 高 | 高 | 不修改现存迁移——只 squash + 冻结，新迁移用新框架 |
| 团队同时维护 5 个方向注意力分散 | 中 | 中 | Sprint 1-2 只做 2 个方向，集中火力 |

---

## 六、总结与建议

### 6.1 最重要的建议

**优先修复方向 A（Defer 活锁）和方向 B（HealthAggregator）**。这两个方向：
1. 实现成本最低（2-3 周）
2. 商业风险最直接（审核停滞 / 降级无告警）
3. 为后续方向提供基础设施（告警通道）

**不要同时开工所有 5 个方向**。每个方向都有其复杂性，同时推进会导致注意力分散和交付延迟。

### 6.2 架构层面的关键判断

| 判断 | 结论 |
|------|------|
| 这 5 个方向是否需要新 crate？ | **否**——全部可在现有模块基础上扩展 |
| 是否需要引入新第三方依赖？ | **否**——tokio + sqlx + Prometheus 已足够 |
| 是否需要新增常驻进程？ | **仅方向 E（Scrubber）需新增 timer**——其他方向复用现有 worker/timer |
| 是否需要改造客户端（Web）？ | **仅方向 C Phase 2（HLC 排序）需改造 ws.js**——其他方向纯服务端 |
| 是否需要停机部署？ | **否**——所有变更设计为向前兼容 |

### 6.3 最终优先级排序

```
P0 (立即)   方向 A - AI Budget Defer 逃生阀
P1 (短中期) 方向 B - System Health Aggregator
P1 (中期)   方向 C Phase 1 - Forward 因果保证
P2 (中期)   方向 E - Data Scrubber (report-only)
P2 (长期)   方向 D - 零停机迁移框架
P2 (长期)   方向 C Phase 2 - 完整 HLC 因果一致性
```

---

**附录**：本文档的每个分析结论都可以追溯回 `docs/requirements/2026-07-11-5-zero-coverage-gaps.md` 中的具体代码锚点和 grep 计数。需要深入某个方向的实现细节时，建议先基于该文档做 Threat Model 分析，再进行接口详设。
