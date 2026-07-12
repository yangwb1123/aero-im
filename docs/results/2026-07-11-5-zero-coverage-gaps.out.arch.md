以下是对 `2026-07-12-five-zero-coverage-systemic-gaps.md` 的全维度架构分析。

---

# 架构分析报告：Aero IM 五个系统性缺口

## 1. 架构评估

### 1.1 当前架构的优势

这五个缺口的发现本身反映了架构在某些维度的成熟度——不是在说系统「烂」，而是在说系统已度过「能跑就行」的阶段，进入了需要系统性运维保障的阶段。

| 优势 | 证据 | 说明 |
|------|------|------|
| **事件驱动骨架清晰** | NATS JetStream + Hub 扇出 + durable/ephemeral consumer 分层明确 | 跨实例扇出的基石稳固，五个 gap 都不是设计方向错误，而是在此基础上的补强 |
| **fail-open 文化** | 6+ 组件独立实现降级 | 说明团队有「可用性优先」的共识，而非把所有组件视为同等关键 |
| **AI 预算系统存在** | per-workspace + global 两级 + weighted cost | 比完全没有预算控制的大多数系统已经领先——方向二的活锁是这个设计的边界情况，不是根本缺陷 |
| **模块边界清晰** | crate 按职责划分，仓储+路由模式一致 | 迁移/scrubber 等系统性改进可以插入而不需要重构整体 |

### 1.2 核心架构局限性

| # | 局限性 | 对应 gap | 架构根源 |
|---|--------|----------|---------|
| **① 事件总线缺少因果层** | 方向一 | `bus/seq.rs` 的设计假设是「每个 subject 独立排序就够了」，缺少跨 subject 的因果依赖追踪。这是 NATS 作为底层基础设施的自然约束，但应用层未做补偿 |
| **② 预算系统缺少反活锁机制** | 方向二 | defer 被设计为「窗口等待」而非「指数退避」，missing a state machine that tracks defer history |
| **③ 故障域彼此隔离但无聚合视图** | 方向三 | 每个组件的 fail-open 是独立实现的——没有统一的 `HealthAggregator` 抽象层。这是「独立演化」架构风格的常见副作用 |
| **④ 迁移作为编译期资产而非运行时能力** | 方向四 | `sqlx::migrate!` 的编译期嵌入是 Rust 生态惯例，但在需要零停机发布时成为约束 |
| **⑤ 数据完整性依赖业务路径** | 方向五 | 系统假设「写入路径正确 = 数据永远正确」，缺少独立的巡检回路由 |

### 1.3 架构债务

**隐性债务（非代码可见，但增加未来变更成本）：**

- **方向一（因果一致性）**：当前 forward 路径在跨房间发布时不带前因——这意味着以后每加一个跨房间操作（share、cross-post、merge），都要重新考虑排序问题。这是一个持续累加的技术债利率。
- **方向四（迁移体系）**：157 个迁移只有 up 没有 down，每增加一个迁移，回滚成本线性增长。到 200+ 迁移时，系统实际上已经无法回滚——一旦上线的 schema 变更必须前向修复。
- **方向五（scrubber missing）**：每加一张新表、一个新 blob 类型，orphan 数据的可能性增加。没有巡检路径的情况下，数据质量只降不升。

**一个值得注意的设计张力**：方向二（defer 活锁）和方向三（级联降级）之间有一个根本矛盾——

> **Defer 的设计初衷是「不要在 budget 耗尽时丢弃 job」，而降级的 fail-open 哲学是「不要在依赖故障时拒绝请求」。两者都优先于「正确失败」，导致系统在持续过载时既不放弃工作也不提示用户。**

这不是单一决策的错误，而是两个好的设计意图在没有协调器的情况下产生了不良的 emergent behavior。

---

## 2. 扩展方向

以下扩展方向排序参考了既有五个 gap 的 P1/P2 优先级，但按「架构变动范围」组织，而非按 gap 编号。

### 方向 A：引入因果排序抽象层（对应 Gap 1）

**为什么需要**：

当前 `per-subject seq` 是基础设施层面的排序，而 `ULID` 是客户端的排序——两者之间缺少**应用层的因果排序**。消息转发（forward）、通知引用（notify）、跨房间 AI 搜索（workspace_ask）都需要因果顺序保障。

直接商业影响：消息可靠性直接影响用户信任，而 Aero IM 作为一个 IM 产品，消息顺序是核心的 UX 属性。

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| **跨 subject 因果跟踪** | 高 | 需要每个消息携带 `(subject, seq)` 前因列表，接收端在依赖未满足时暂存。暂存区的大小和超时策略是难点 |
| **HLC 精度 vs 性能** | 中 | HLC 需要在每消息上附加 `(wall_clock, logical_counter, node_id)` 三元组，约 24 byte。WebSocket 帧的带宽开销可控，但序列化/反序列化的计算开销值得 bench |
| **向后兼容** | 中 | 旧客户端不会因果排序，需要在服务端降级（新服务端给旧客户端发全序的帧） |

**架构变更范围**：

```
┌─────────────────────────────┐
│  新：CausalOrdering Layer   │ ← 跨 subject 的因果跟踪
│  ┌───────────────────────┐  │
│  │ HLC clock (per node)  │  │ ← 替代纯 ULID 排序
│  ├───────────────────────┤  │
│  │ CausalityTracker      │  │ ← per-room 因果 DAG
│  ├───────────────────────┤  │
│  │ Stash (bounded)       │  │ ← 等待前因的暂存区
│  └───────────────────────┘  │
└──────────┬──────────────────┘
           │ 附加因果头
           ▼
┌─────────────────────────────┐
│  现有：EventBus (NATS)      │ ← per-subject seq 保留为去重键
│  im.room.{id} / live.stream.{id}
└─────────────────────────────┘
```

**对现有系统的影响**：

- **对 `forward.rs`**：最显著的变化。发布前需要从源消息提取前因并附加到目标消息。
- **对 `bus/seq.rs`**：保持不变——per-subject seq 继续作为去重和客户端排序回落方案。
- **对 `ws.js`**：客户端需要新的排序引擎（从 `_lastSeen > mid` 切换到因果 DAG）。
- **对 `run_bus_listener`**：解码后增加因果暂存逻辑。

**关键设计决策：服务端暂存 vs 客户端暂存**

| 选项 | 优势 | 劣势 |
|------|------|------|
| **服务端暂存**（推荐） | 控制暂存区大小；兼容旧客户端；统一的排序输出 | 增加 Hub 扇出的内存开销；需要 bounded stash + 超时 flush |
| **客户端暂存** | 减轻服务端状态负担；每个客户端按需等待 | 旧客户端无法排序；WebSocket 断开后状态丢失；需要服务端发送依赖清单 |

**建议**：服务端做基础的 per-room 因果排序（保证房间内有序），客户端做跨房间的最终排序。这样服务端的暂存区是 bounded per-room（≤500 条），客户端承担跨房间的复杂排序。

### 方向 B：统一降级/健康仪表盘（对应 Gap 3 + 部分 Gap 2）

**为什么需要**：

6+ 独立 fail-open 路径的叠加效果在故障场景下会产生「自激振荡」（Redis 不可用 → 限流 fail-open → PG 过载 → 更多组件 fail-open）。没有聚合的 `aero_system_health` gauge，运营在一个 P1 事件中需要检查 6+ 面板才能确认系统处于降级模式。

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| **降级因果关系追踪** | 高 | 区分「因」降级（Redis down 导致限流失效）和「果」降级（PG 过载导致 AI 查询失败），避免重复告警 |
| **非二进制状态** | 中 | 降级不是 on/off——RateLimit 降级 70% vs 100% 的语义不同 |
| **自激振荡抑制** | 中 | 需要 fail_open_cooldown（进入降级后 5s 冷却期），避免限流在 on/off 之间高频振荡 |

**架构变更范围**：

```
┌─────────────────────────────────────┐
│  新：HealthAggregator（常驻协程）     │
│  ┌─────────────────────────────┐    │
│  │ 上游：ComponentHealthProbe  │    │ ← 每个组件注册 probe 函数
│  ├─────────────────────────────┤    │
│  │ 中游：HealthStateMachine    │    │ ← 降级状态机（normal/degraded/critical）
│  ├─────────────────────────────┤    │
│  │ 下游：HealthExporter        │    │ ← Prometheus + WS 帧 + admin API
│  └─────────────────────────────┘    │
└─────────────────────────────────────┘
          ▲ 注册 probe               │
          │                          ▼
┌─────────┴──────────┐    ┌──────────────────────┐
│ 现有组件（6+）       │    │ 降级信号 → WS 客户端   │
│ 每个嵌入 probe()    │    │ {"type":"degraded",   │
│ 返回 health status  │    │  "service":"ai",      │
└────────────────────┘    │  "mode":"deterministic"}│
                          └──────────────────────┘
```

**对现有系统的影响**：

- **每个 fail-open 组件加 probe**：在现有 fail-open 路径的每个 `warn!` 处加一个 `HealthAggregator::report_degraded` 调用。最小侵入。
- **WS 帧扩展**：新增 `degredation` 帧类型——这需要前端对应处理（黄色提示条）。
- **无现有代码重写**：所有 fail-open 路径只增加告警上报点，不改逻辑。

**关键设计决策：推 vs 拉模式**

| 选项 | 优势 | 劣势 |
|------|------|------|
| **推（组件主动 report）**（推荐） | 实时性高；组件最清楚自己的状态 | 每个组件需要 health aggregator 的引用；组件可能忘记 report；风暴问题（高频切换） |
| **拉（HealthAggregator 轮询）** | 组件无感知；统一节奏 | 实时性取决于轮询间隔；组件可能处于「看起来健康但实际降级」的状态（网络分区场景） |

**建议**：混合——组件在降级/恢复时**推**一个事件（轻量），`HealthAggregator` 同时以 10s 间隔**拉**各组件的 `is_healthy()` 方法确认状态。推用于快速响应，拉用于兜底检测。

### 方向 C：AI 管线预算逃生阀（对应 Gap 2）

**为什么需要**：

defer 活锁是一个「概率性的正确性问题」——不是每次都触发，但在持续高负载下必然触发。AI 审核合规性是 P0 级需求（用户举报的消息必须在 SLA 内完成审核），活锁意味着审核作业「可能无限期延迟」。

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| **逃生阀 vs 预算系统互斥** | 中 | 逃生阀（defer_count > MAX → 强制运行）本质上绕过了预算——需要在强制运行后标记为「超预算执行」并在下一个窗口补偿 |
| **单个 workspace 淹没全局** | 中 | 一个大 workspace 的 Embed job（weight 1）可能耗尽全局 budget 导致其他 workspace 的 Moderation（weight 2）被 defer。逃生阀需要 per-workspace 优先级 |
| **逃生阀的阈值选择** | 低 | MAX_DEFERS 太短（如 3）→ 频繁绕过预算；太长（如 20）→ 活锁持续时间过长。需要通过 operational data 校准 |

**架构变更范围**：

```
┌───────────────┐     ┌──────────────────────┐
│ AiWorker       │     │ ai_jobs 表            │
│ claim() ───────┼──►  │ + defer_count        │
│ defer()        │     │ + budget_overage     │
│ forced_run() ──┼──►  │ + priority (mod/embed)│
└───────┬───────┘     └──────────────────────┘
        │
        │ 逃生阀路径
        ▼
┌──────────────────────────────────────────────┐
│ if defer_count >= MAX_DEFERS (如 10):         │
│   - 强制运行（不检查 budget）                   │
│   - 标记 budget_overage = true                │
│   - 下一个窗口的 budget 减去这个 overage        │
│   - 发出告警：aero_ai_force_run_total         │
└──────────────────────────────────────────────┘
```

**对现有系统的影响**：

- **`ai_job.rs`**：加 `defer_count` 列（迁移），修改 `defer()` 方法递增该列。
- **`worker/mod.rs`**：在 defer 循环中加入逃逸阀判断——在 `defer` 之前检查 `defer_count`，超阈值走强制运行。
- **`aero-ai` 仓储**：加 `select_deferred_count` 查询，用于 Gang scheduling 决策。

**关键设计决策：补偿策略**

| 选项 | 优势 | 劣势 |
|------|------|------|
| **下一窗口扣除**（推荐） | 长期总量受控；公平性较好 | 如果持续过载，扣除永远不会被消耗（窗口永远 in deficit） |
| **按比例预留** | 保证强制运行不影响正常作业 | 预留比例的选择是纯猜测（5%/10%/20%）；预留不足时仍会活锁 |
| **Defer 超时 + DLQ** | 设计最简洁；避免无限 defer | 高负载场景下 job 会大量进入 DLQ，运营负担大；区分「真的死信」和「只是预算不足」需要额外逻辑 |

**建议**：**下一窗口扣除 + 比例预留的组合**。保留 10% 的 budget 给强制运行，在强制运行消耗的 budget 从下一窗口扣除（最多扣到 budget 的 50% 上限）。这样既有紧急通道又不会无限累积欠账。

### 方向 D：零停机迁移框架（对应 Gap 4）

**为什么需要**：

157 个 up-only 迁移意味着系统已经不可能回滚——任何一个新迁移上线后，如果引发问题，唯一的恢复手段是**从备份重建**。对于 99.99% SLA 目标，这是不可接受的。而且消息表分区作为明确的停机操作，说明可观测性设计还没有覆盖到大表运维。

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| **Expander/Contractor 模式的 Rust 集成** | 高 | `sqlx::migrate!` 的编译期嵌入意味着迁移和二进制是紧耦合的——一个二进制只能支持一个 schema 版本 |
| **消息表分区** | 高 | messages 表有多个外键关联（reactions, notifications, ...），分区需要级联调整 |
| **迁移锁定** | 中 | 多实例并行启动时，迁移需要分布式锁（Postgres advisory lock + 超时） |
| **down.sql 的工作量** | 中 | 157 个迁移写 down 需要枚举所有 schema 变化；有些迁移可能无法恢复（如 `DROP COLUMN`） |

**架构变更范围**：

```
当前：                       目标：
┌─────────────────┐        ┌────────────────────────┐
│ sqlx::migrate!  │        │ Schema Version Registry │
│ 编译期嵌入       │        │ 运行时检查期望版本       │
│ 启动时自动运行   │        │ 版本不匹配 → fail-fast  │
└─────────────────┘        └────────────────────────┘

迁移流程变更：
V1（快速）：每个迁移加 down.sql + 版本 API
V2（零停机）：Expander（加列，旧版兼容）→ 部署新二进制
              → Contractor（删旧列，仅在新版运行时）
V3（自动化分区）：通用 partition_table() 工具函数
```

**关键设计决策：扩展策略**

| 选项 | 优势 | 劣势 |
|------|------|------|
| **逐步改进（推荐）** | 先加 down.sql 和版本 API（低风险），再逐步引入 Expander/Contractor | 分区仍需要停机窗口（在 V2/V3 完成前） |
| **一次性大改** | 架构一步到位 | 157 个迁移重写 down 的时间成本极高；容易引入回归 |
| **放弃零停机，专注可回滚** | 降低目标复杂度；大多数系统其实不需要零停机 | 无法承诺企业 SLA（99.99%）；分区操作仍需要窗口 |

**建议**：分三个阶段——Phase 1：为最近 30 个迁移加 down.sql + 版本 API（2 周）。Phase 2：引入 Expander/Contractor 模式用于新迁移（3 周）。Phase 3：通用分区工具 + 消息表分区（6 周）。

### 方向 E：数据完整性巡检体系（对应 Gap 5）

**为什么需要**：

无 scrubber 的数据系统本质上是「相信写入路径不出错」。在 blob 无 checksum、reaction 无级联清理、删除 participant 不校验引用的情况下，数据质量的衰减是无声的、累积的。对于监管合规（HIPAA、SOC2）场景，scrubber 是必不可少的。

**核心挑战**：

| 挑战 | 难度 | 说明 |
|------|------|------|
| **只读 vs 修复的边界** | 中 | scrubber 的默认模式是只读报告——但「报告了但不修」的运维价值有限 |
| **节流而不阻塞** | 中 | 每 24h 扫描 blobs 表（可能数百万行），需要节流不干扰主路径 |
| **Checksum 的写路径影响** | 低 | 上传时计算 SHA-256 的 CPU 开销（大文件）和存储开销（256 bit/行） |
| **跨存储后端的一致性** | 中 | LocalFs 的校验逻辑和 S3 的校验逻辑不同（S3 有 ETag，但 ETag 在大文件 multipart 上传时不等于 MD5） |

**架构变更范围**：

```
┌────────────────────────────────────┐
│  新：Scrubber Worker（可配置间隔）   │
│  ┌──────────────────────────────┐  │
│  │ ScrubPlan: 每次运行时定义    │  │
│  │ - 检查哪些资源（blob/msg/   │  │
│  │   reaction/notification）    │  │
│  │ - 模式：report-only / fix   │  │
│  │ - 节流：burst/sec           │  │
│  └──────────────────────────────┘  │
│  ┌──────────────────────────────┐  │
│  │ ScrubExecutor:              │  │
│  │ - blobs: checksum + ref     │  │
│  │ - reactions: msg exists     │  │
│  │ - notifications: msg exists │  │
│  │ - participants: rows refer  │  │
│  └──────────────────────────────┘  │
│  ┌──────────────────────────────┐  │
│  │ ScrubReporter:              │  │
│  │ - Prometheus metrics        │  │
│  │ - audit log                 │  │
│  │ - admin API (GET /scrub)    │  │
│  └──────────────────────────────┘  │
└────────────────────────────────────┘
```

**对现有系统的影响**：

- **`blob.rs`**：加 `sha256` 列（可选，向后兼容），新上传的 blob 记录 SHA-256。
- **`BlobStore` trait**：加 `verify(id, expected_size, expected_checksum) -> Result<bool>` 方法（带默认实现——对不存在 checksum 的行跳过）。
- **`blob_gc_queue`**：scrubber 发现的 orphan blob 可复用该队列。
- **无现有业务路径修改**：scrubber 是纯增量。

**关键设计决策：完整性边界**

| 选项 | 优势 | 劣势 |
|------|------|------|
| **严格模式**（每个 blob 都校验 checksum） | 最强的数据完整性保证 | 大文件（>100MB）的 SHA-256 计算有显著 I/O 开销；S3 需要额外的 GET 请求 |
| **抽样模式**（每批随机抽样 10%） | 低开销，覆盖大部分数据问题 | 不能保证发现所有问题；随机抽样可能错过长期积累的静默损坏 |
| **引用完整优先**（只校验引用不校验 checksum） | 最容易快速落地；覆盖最核心的场景（orphan blob/message） | 文件内容损坏无法发现 |

**建议**：**先做引用完整性**（2 周），再加 checksum（可选写路径，2 周），然后逐步开启校验（只对新 blob 开始计算——旧 blob 的 checksum 在第一次 scrub 时计算并回填）。

---

## 3. 接口设计建议

### 3.1 关键模块的接口原则

**原则一：每个新增基础设施模块（CausalOrdering/HealthAggregator/Scrubber）都应该是可选组件**

原因：五个 gap 的修复都不应该影响核心业务路径的性能。因果排序如果导致消息延迟增加 2ms，在未启用因果功能的租户上不应该有感知。

实现方式：所有新组件通过 feature flag 或 `Option<T>` 接入——

```
// 伪代码模式
pub struct ImService {
    causal_tracker: Option<Arc<CausalTracker>>,
    health_aggregator: Option<Arc<HealthAggregator>>,
    scrubber: Option<Arc<Scrubber>>,
}

impl ImService {
    pub fn publish_room_event(&self, room: &Room, event: &RoomEvent) {
        // 核心路径：无变化
        let seq = self.bus.next_seq(&subject).await;
        // 可选：因果跟踪
        if let Some(tracker) = &self.causal_tracker {
            tracker.attach_causality(event, seq, &subject).await;
        }
        self.bus.publish(&subject, event).await;
    }
}
```

**原则二：告警/健康/观察路径使用 trait object 而非具体类型**

原因：避免循环依赖——`HealthAggregator` 不应该知道所有组件的具体实现。

```
// 伪代码
#[async_trait]
pub trait ComponentHealth: Send + Sync {
    fn component_name(&self) -> &'static str;
    async fn is_healthy(&self) -> ComponentHealthStatus;
    async fn is_degraded(&self) -> Option<DegradationInfo>;
}
```

每个组件（`rate_limit::RateLimiter`、`AiBackend`、`push::PushGateway`）实现这个 trait，`HealthAggregator` 通过 `Vec<Box<dyn ComponentHealth>>` 聚合。

**原则三：scrubber 的 repair 路径必须是可逆的**

原因：自动修复永远有风险——scrubber 的默认模式是 `report-only`，fix 模式需要 `AERO_SCRUBBER_FIX_MODE=1` 显式开启，并且每次修复操作都必须有 `undo` 信息（记录到 `scrubber_actions` 表）。

### 3.2 是否需要新的抽象层

| 抽象层 | 需要 | 理由 |
|--------|------|------|
| **HealthAggregator** | **是** | 6+ 组件的降级状态需要统一归集——当前每个组件的实现是手写的 `if err { warn!() }`，没有通用抽象 |
| **CausalOrderingLayer** | **是（但轻量）** | 不需要完整的 CRDT 框架，只需要一个 per-room 的因果暂存区 + HLC 时钟 |
| **Scrubber Framework** | **是** | 引用完整性的检查逻辑在 blob/message/reaction/notification 上重复出现，需要统一的 `ScrubCheck<T>` trait |
| **Migration Framework** | **否（改进现有）** | 不需要替换 `sqlx::migrate!`，而是在其上包装 Expander/Contractor 工具函数 |

### 3.3 向后兼容策略

| Gap | 兼容策略 | 说明 |
|-----|----------|------|
| 方向一（因果） | HLC 消息中带 `causal_version` 字段，旧客户端忽略该字段 | 服务端对所有客户端发送同一帧，只是客户端侧的排序算法不同 |
| 方向二（defer 逃生阀） | 新 `defer_count` 列有 DEFAULT 0，旧版 AiWorker 不读写该列 | 完全向后兼容 |
| 方向三（健康聚合） | 新 WS 帧 `degradation` 类型——旧客户端直接忽略不识别的帧类型 | WS 框架已有 `ignore_unknown` 模式 |
| 方向四（迁移） | `AERO_DB_SCHEMA_VERSION` 的默认值是 `latest`（兼容旧行为） | 旧版二进制不设该变量，行为不变 |
| 方向五（scrubber） | `sha256` 列 DEFAULT NULL——写路径可选 | 新 scrber 对 NULL 行跳过 checksum 校验 |

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

| Gap | 建议引入 | 理由 | 替代方案 |
|-----|---------|------|---------|
| 方向一（因果） | **不引入**——自实现 HLC + 因果 DAG | HLC 的逻辑简单（~200 行），依赖纯 Rust 标准库 | HyParView（CRDT 框架）/ Amazon DynamoDB 的 HLC 实现——太重 |
| 方向二（defer 逃生阀） | **不引入**——存量 PG `ai_jobs` 表加列 | 纯 schema 变更 + 逻辑变更，无新依赖 | Redis 延迟队列——增加故障面，不划算 |
| 方向三（健康聚合） | **不引入**——基于 Prometheus metric 即可 | 已有 Prometheus + Grafana 栈，聚合 gauge 是原生能力 | OpenTelemetry Collector 的 alerting rules——可用但当前场景简单 |
| 方向四（迁移） | **考虑 `refinery`** 作为 `sqlx::migrate!` 的补充 | `refinery` 支持 `down.sql` 和版本分组——但当前迁移体系已嵌入 sqlx，改框架成本高 | 直接改进 sqlx 的迁移包装器（加 down.sql 校验和 Expander 工具） |
| 方向五（scrubber） | **不引入**——自实现 | scrber 的核心逻辑是 SQL 查询 + 比较 + 报告，不需要专用框架 | pg_checksum / pg_repack——需要 PG extension，不适用于 S3 blob |

**唯一值得考虑的引入**：如果方向四（零停机迁移）成为 P0 级需求，应该评估 `refinery` 是否能在不重写 157 个迁移的前提下提供 down 支持。大概率答案是否定的——迁移框架切换的成本远高于在 `sqlx::migrate!` 上加一个包装层。

### 4.2 第三方依赖评估标准

对于这五个 gap 对应的任何引入决策，应该在以下四个维度上评分（1-5）：

| 维度 | 权重 | 说明 |
|------|------|------|
| **依赖性风险** | 40% | 依赖是否维护活跃？是否和当前 tokio/sqlx 版本兼容？是否属于「大依赖（如 tokio/core crates）」？ |
| **抽象匹配度** | 30% | 依赖解决的问题域是否和 Aero IM 的问题域完全对齐？还是需要做适配层？ |
| **运维负担** | 20% | 引入后是否需要额外的守护进程？是否需要额外的数据存储？是否增加部署复杂度？ |
| **学习成本** | 10% | 团队是否熟悉？文档是否完善？ |

**以 HLC 为例**：

| 维度 | 评分 | 说明 |
|------|------|------|
| 依赖性风险 | 5/5 | 自实现 HLC 不引入外部依赖 |
| 抽象匹配度 | 5/5 | 完全对齐——HLC 就是为分布式因果排序设计的 |
| 运维负担 | 5/5 | 不增加守护进程或存储 |
| 学习成本 | 4/5 | HLC 概念需要团队理解，但实现简单 |
| **总分** | **4.7/5** | |

**以 refinery 迁移框架为例**：

| 维度 | 评分 | 说明 |
|------|------|------|
| 依赖性风险 | 3/5 | 活跃但小众（github 1.5K star）；与 sqlx 集成需要适配层 |
| 抽象匹配度 | 3/5 | 支持 down.sql 但不支持 Expander/Contractor 模式 |
| 运维负担 | 4/5 | 不增加守护进程，但需要迁移脚本格式调整 |
| 学习成本 | 4/5 | 与 sqlx 迁移类似的 DSL |
| **总分** | **3.3/5** | |

### 4.3 自建 vs 采购

这不适用于当前场景——五个 gap 全部是「系统内修补」而非「新能力采购」。没有外部 SaaS 产品能解决「跨 NATS subject 的因果一致性」或「AiWorker 的 defer 活锁」。

唯一的例外是方向三（级联降级）的告警部分——如果 Aero IM 未来集成 Datadog/New Relic/PagerDuty，可以用外部告警平台处理 `aero_system_health` metric 的告警规则。但 metric 的生成逻辑仍需要自建。

---

## 5. 实施路线图

### 5.1 优先级总表

| 优先级 | Gap | 工期估计（单人） | 依赖 | 商业价值 / 风险比 |
|--------|-----|----------------|------|-------------------|
| **P0** | 方向二：AI defer 活锁 (defer_count + 逃生阀) | 1-2 周 | 无 | 最高。AI 审核的合规性是目前最接近「数据丢失」的风险——defer 活锁意味着审核作业无限期不执行。低成本修补 |
| **P1** | 方向三：健康聚合 (fail-open 告警) | 2-3 周 | 无 | 高。故障 MTTR 减少 10x，运营自愈能力大幅提升。且为方向二的逃生阀提供告警通道 |
| **P1** | 方向一：因果一致性 (HLC + 暂存区) | 4-6 周 | 方向四的部分前置 | 高用户可见性，但实现复杂度高。占位实现（HLC stamp + 服务端无暂存区）可压缩到 2 周 |
| **P2** | 方向五：scrubber (引用完整优先) | 3-4 周 | 无 | 中等。数据质量是长期运维保障，非关键路径依赖 |
| **P2** | 方向四：零停机迁移 (Phase 1: down.sql + 版本 API) | 2-3 周 | 无 | 中等。企业 SLA 的前提条件，但短期不触发 |

### 5.2 阶段划分

**Phase 0（1-2 周）：Quick Wins**

目标：消除最可能在生产中引发数据损失或合规事故的风险。

```
方向二（defer 逃生阀）：
  - ai_jobs 表加 defer_count DEFAULT 0（迁移 0158）
  - defer() 方法递增 defer_count
  - 当 defer_count >= MAX_DEFERS（10）时：强制运行 + budget_overage 标记
  - Prometheus 指标 aero_ai_force_run_total
  - Grafana 看板：defer_count 分布、force_run 频次

方向三（健康聚合的最低可行版本）：
  - 定义 ComponentHealth trait（~50 行）
  - 在 rate_limit、AI、push、moderation 上加 probe
  - Prometheus gauge aero_system_health_mode{component="..."}
  - 简单告警规则：any component degraded > 5min → P2 alert
```

**Phase 1（3-5 周）：核心架构改进**

目标：建立故障可观测和消息顺序可保证的基础。

```
方向一（因果一致性 — 最小实现）：
  - HLC 实现（~200 行，纯 Rust，无依赖）
  - 每消息附加 HLC 时间戳（RoomEvent 新增字段）
  - 服务端不设暂存区——暂存在 Phase 2 实现
  - ws.js 的 _lastSeen 从 ULID 字典序改为 HLC 序列化结果
  - 降级：HLC 不可用退回 ULID

方向三（健康聚合 — 完整实现）：
  - fail_open_cooldown 机制（5s 冷却）
  - WS 帧 degradation（服务端推送，前端黄色提示条）
  - 自激振荡检测：两个以上关联组件同时 degraded → P1 告警
  - admin API: GET /api/admin/health 返回聚合状态
```

**Phase 2（6-10 周）：完整方案**

目标：解决剩余复杂问题，建立运维自动化的闭环。

```
方向一（因果一致性 — 完整实现）：
  - 服务端 per-room 因果暂存区（bounded 500）
  - 暂存超时（5s）自动 flush
  - forward.rs 跨房间发布携带 causality header
  - 监控：stash_size、stash_timeout_total、causal_violation_total

方向四（零停机迁移 — Phase 1）：
  - 最近 30 个迁移加 down.sql
  - make migrate-down 验证完整降级链
  - GET /api/debug/schema-version API
  - 迁移锁定（Postgres advisory lock）

方向五（scrubber — 引用完整优先）：
  - blob GC 队列复用——scrubber 发现 orphan blob 自动入队
  - 消息/reaction/notification 引用校验
  - Prometheus 审计指标
  - admin API: GET /api/admin/scrub
```

**Phase 3（11-16 周）：企业级加固**

目标：达到 99.99% SLA 和合规要求。

```
方向四（零停机迁移 — Phase 2/3）：
  - Expander/Contractor 工具函数
  - 消息表分区（安全模式，复用 stream_viewer_samples 的迁移策略）
  - canary 部署支持（AERO_DB_SCHEMA_VERSION）
  - 蓝绿部署场景验证

方向五（scrubber — checksum 完整实现）：
  - blob 表加 sha256 列（写路径可选）
  - BlobStore trait 加 verify() 方法（带默认 skip 实现）
  - 全量 scrubber 开启（24h 间隔）
  - HIPAA/SOC2 合规报告输出

方向一（跨区域支持）：
  - HLC 跨区域兜底（physical clock 误差 50-200ms → logical counter 空间扩展）
  - 跨区域暂存区超时调整（从 5s 到 30s）
```

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **方向二的逃生阀导致 budget 失控** | 低 | 中 | 强制运行时记录 `budget_overage`，在下一窗口补偿（最多抵扣 50%）；Grafana 看板实时显示 |
| **方向一的暂存区 OOM** | 中 | 高 | Bounded stash（500/room）+ 超时 flush（5s）；监控 stash_size 并告警上限；超限时 force flush |
| **方向一的 HLC 在跨区域部署下的精度** | 中 | 中 | 跨区域部署前在 Phase 2 验证 HLC 在 50-200ms 误差下的行为；需要测试夹具 |
| **方向三的降级告警风暴** | 中 | 中 | `fail_open_cooldown`（5s 冷却期）+ 告警抑制规则（同一组件 15 分钟内只发一次告警） |
| **方向四的 down.sql 不完全** | 高 | 高 | 只对**最近 30 个**迁移写 down.sql，而不是所有 157 个。历史迁移的 down 通过「回滚到上一个备份」兜底 |
| **方向五的 scrber 误报** | 中 | 低 | 默认 report-only 模式；fix 模式需要环境变量显式开启；每次 fix 写入 `scrubber_actions` 审计表 |
| **五个方向并行开发时的冲突** | 中 | 中 | `AGENTS.md` 的映射很清晰——如果五个方向分配给不同 agent，确保 `routes.rs` 的 `.merge` 链和 `lib.rs` 的 `pub mod` 按约定走，避免冲突 |

---

## 总结

五个 gap 的共性模式是「**系统已经成熟到暴露了运维层面的结构裂缝**」——不是因为设计错误，而是因为架构演化到一定规模后，一些在小型系统中可以容忍的简化假设（per-subject seq 就够了、fail-open 各管各的、迁移 never roll back）开始产生实际的运维成本。

最值得优先投入的是 **Phase 0 的 Quick Wins**（方向二的 defer_count + 方向三的最小健康 gauge）——它们不改变系统架构，成本低、风险低，但立即消除两个 P1 缺口。这符合「先止血再手术」的原则。
