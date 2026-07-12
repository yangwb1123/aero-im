Now I have a thorough understanding of the codebase architecture. Here is my full analysis:

---

# 架构分析报告：Aero IM 生产就绪性评估

## 1. 架构评估

### 1.1 现有优势

系统架构有一系列值得肯定的设计决策：

**事件驱动 DAG 选择正确。** NATS JetStream 作为跨实例事实源（durable consumer `aero-server`），配合进程内 Hub bounded mpsc 扇出，是经过验证的高吞吐实时架构。将房间事件（`im.room.*`）和直播事件（`live.stream.*`）分离到不同耐久性消费者合约（durable vs ephemeral），体现了对不同语义需求的清晰认知。

**Crate 分层健康。** 依赖图自底向上（`aero-common` → `aero-bus` → `aero-storage` → `aero-auth` → ... → `aero-server`），无环。每个 crate 有明确的职责边界。`AGENTS.md` 的 crate 地图与 `Cargo.toml` 的 workspace members 一致。

**Hub 背压设计成熟。** 使用 bounded `mpsc` + `WsConfig::disconnect_on_full` 的 OOM 防护 + 丢帧时的 `resync` 帧恢复——这是生产级 WebSocket 扇出的标准模式。

**Boot 模块化好。** `main.rs` 拆分为 12 个 boot 子模块（`persistence.rs`, `repos.rs`, `services.rs`, `ingest.rs`, `orchestration.rs`, `state_builder.rs`, `background.rs`, `metrics_tasks.rs`, `retention.rs`, `serve.rs`, `shutdown.rs`, `helpers.rs`），每个职责单一，main.rs 保持为编排胶水。

**MLS/E2E 边界清晰。** `common/src/mls.rs` 仅不透明字节 scaffold，无 openmls 依赖——server 不触碰客户端密码学边界，是正确决定。

### 1.2 核心架构债务

验证结果揭示了三层架构债务，按影响面排列：

#### 债务一（P0）：后台任务生命周期管理的架构真空

这是系统最严重的架构缺失。**问题在模式层面，不在实现疏忽。**

```
当前状态：
  tracker.spawn(async move {
      if let Err(e) = some_bot::run(state).await {
          tracing::error!(...);
      }
  });
  // ↑ 失败 = 永久消亡，无人知晓，无法恢复
```

关键证据：
- **7 个 bot 不接收 `CancellationToken`**（`agent_bot`, `ooo_bot`, `push_bot`, `golive_bot`, `transcribe_bot`, `moderation_bot`, `unfurl_bot`）
- **`run_bus_listener` 和 `run_live_bus_listener` 也不接收 token**——这是最核心的消息泵，如果挂掉，客户端连接成功但收不到任何事件，而 `/health/ready` 依然返回 200
- 仅有 `ai_shutdown` token 传递的定时器类任务（`scheduled.rs`, `digests.rs`, `me_export.rs`, `retention.rs` 等）能优雅关闭，但无法被自动恢复
- 没有任务健康信号。`/health/ready` 只探测 PG/Redis/NATS/blob，**不探测一条消息循环是否活着**
- 启动时序窗口：`spawn_background`（step 10）返回后立即 `serve`（step 13），NATS consumer 可能在 HTTP listener 绑定后才完成注册——期间事件丢失

**这不是"少传了个参数"的问题，而是缺少了后台任务的监控合约（observability contract）、恢复策略（recovery strategy）、和生命同期契约（lifecycle contract）。**

#### 债务二（P0）：配置管理的去中心化

45+ 个 `env::var()` 遍布 boot 模块，没有统一配置抽象。具体症状：

- **命名不统一**：`AERO__SERVER__SAVED_SEARCH_MONITOR_SECS`（figment 双下划线风格）和 `AERO_AI_MODERATION`（单下划线风格）在同一文件共存
- **无启动验证**：拼写错误的 env var 静默降级功能（如 `AERO_UNFURL` 少打个字母，unfurl_bot 不启动且无日志）
- **无运行时视图**：没有 `/debug/config` 端点，运维人员无法查看生效配置
- **重复样板**：每个模块重复 `std::env::var("AERO_X").ok().and_then(|s| s.parse().ok()).unwrap_or(default)` 模式
- **配置散布**：`retention.rs` 单独 13 个 env var 调用，每个带 `unwrap_or()`

问题在于 figment 已经提供了分层配置加载（默认 → `config.toml` → 环境变量），但大量模块绕过 figment，直接读环境变量。这意味着 `config.toml` 中配置的项可能和直接 `env::var()` 读取的项不一致——运维人员无法通过一份配置清单了解系统全部可配置项。

#### 债务三（P1）：模块间耦合模式——依赖注入不完整

```
// 正面案例（已解耦）：background.rs 依赖 state（封装好的 AppState）
// 反面案例：每个 bot 内部自己做 env::var() 解释
```

`AppState` 是好的开始（集中了 PG/Redis/NATS/Hub/presence/viewers 等），但配置没有被纳入 state。每个 bot/模块各自解析自己的环境变量，导致：
- 单元测试时无法通过注入配置来模拟不同场景
- 无法在单一位置审计所有配置项的变更
- 配置变更的影响面不可追踪

### 1.3 设计决策合理性评估

| 决策 | 合理性 | 说明 |
|------|--------|------|
| NATS JetStream 作为跨实例事件总线 | ✅ 正确 | at-least-once + per-subject seq 适合 IM 语义 |
| Hub 进程内 bounded mpsc 扇出 | ✅ 正确 | 避免跨 WS 连接的 mutex 争用，bounded 防 OOM |
| 直播事件用 ephemeral consumer | ✅ 正确 | 丢失弹幕可接受，每实例独立消费 |
| 集群状态走 Redis sorted-set | ✅ 正确 | 跨实例共享，天然 TTL 过期 |
| boot 模块拆分 | ✅ 正确 | 但 spawn_all 返回后即 serve 需要修复 |
| 无 CancellationToken 的 bot 实现 | ❌ 架构缺失 | 缺少生命周期合约 |
| 配置读取分散 | ❌ 架构债务 | 本应全部通过 figment |
| docs/requirements/ 过度积累 | ❌ 元债务 | 372 份分析 vs 1 份 runbook |

---

## 2. 扩展方向

### 方向一：后台任务治理框架（P0，高优先级）

**为什么需要：** 当前 23 个后台任务运行在"发射后不管"模式。在 Kubernetes 环境中，这会表现为：pod 正在运行（HTTP 200），但所有 bot 都已经挂掉，用户收不到推送和 AI 回复。没有运维能发现。

**核心挑战：**
- 需要在不改变现有 bot 签名的情况下增加生命周期感知（向后兼容）
- 健康信号必须与 `/health/ready` 整合
- 重启策略需要区分瞬态错误（重试）和永久错误（死信/降级）

**预期的架构变更：**

```
┌─────────────────────────────────────────────────┐
│                 Supervisor Layer                 │
│  ┌──────────┐  ┌──────────┐  ┌───────────────┐ │
│  │TaskHandle │  │HealthReg │  │ RestartPolicy │ │
│  │(cancel +  │  │istry     │  │ (retry/backoff│ │
│  │ join)     │  │(heartbeat│  │  dead-letter) │ │
│  └──────────┘  └──────────┘  └───────────────┘ │
└─────────────────────────────────────────────────┘
         │                │               │
         ▼                ▼               ▼
┌─────────────────────────────────────────────────┐
│            Spawn with lifecycle wrapper          │
│  spawn_managed(tracker, "agent_bot", run_fn,    │
│                cancel, health_check, restart)   │
└─────────────────────────────────────────────────┘
```

**关键接口：**
- `ManagedTask { cancel: CancellationToken, join: JoinHandle, health: HealthSignal }`
- `HealthSignal::alive()` / `HealthSignal::failed(error)` —— 由任务定期调用
- 每个 bot 的 `run()` 函数增加 `cancel: CancellationToken` 参数
- `/health/ready` 增加 `background_tasks` 字段

**对现有系统的影响：**
- 最小侵入：`supervisor` 作为新模块，在 `spawn_background` 处包装现有 spawn 调用
- 需要为每个 bot 添加 `CancellationToken` 参数（7 个 bot + 2 个 bus listener）
- 不需要改变 bot 的逻辑——仅在 `background.rs` 增加 wrapper
- `/health` 响应额外返回 `{"background_tasks": {"total": 23, "alive": 23, "dead": 0}}`

### 方向二：统一配置管理层（P0，高优先级）

**为什么需要：** 消除配置的碎片化，让运维人员能通过单一接口了解生效配置。这是生产可运维性的基础。

**核心挑战：**
- 存量代码大量直接调用 `env::var()`，需要逐步迁移
- 有些配置是在 struct 构建时解析的，有些是在运行时每次调用的
- 需要保持向后兼容——不能破坏现有 `AERO_*` 环境变量

**预期的架构变更：**

```
方案 A（推荐）：配置注册表 + 迁移门
  1. 在 AppConfig 中增加一个 RuntimeConfig 结构，包含所有运行时可变的配置项
  2. AppState 持有 Arc<RuntimeConfig>，所有模块通过 state.config 读取
  3. 新增 /debug/config (admin-only) 返回序列化的 RuntimeConfig
  4. 迁移路径：env::var() → state.config.X，逐个模块迁移，不一次改完

方案 B（激进）：NATS-config 驱动热加载
  1. 配置存储迁移到 NATS KV Store
  2. 监听配置变更事件，运行时热更新
  3. 需要更复杂的一致性保证——适合 P2 阶段

权衡：方案 A 复杂度低，适合 P0；方案 B 更适合 P2 阶段在途重构
```

**对现有系统的影响：**
- 方案 A 完全向后兼容——现有 `env::var()` 代码可以继续运行，逐步迁移
- 新增 `RuntimeConfig` struct，`AppState` 增加 `config: Arc<RuntimeConfig>`
- 每个迁移的模块更新其配置读取路径
- `truth-check.sh` 可配置 whitelist 规则禁止新增 `env::var()` 调用

### 方向三：启动就绪信号通道（P1，中优先级）

**为什么需要：** 当前的 13 步线性启动存在两个窗口：（1）HTTP listener 绑定但 NATS consumer 未就绪时，事件丢失；（2）后台任务正在初始化但 health endpoint 已返回 ready。

**核心挑战：**
- NATS `subscribe()` 是异步的——`subscribe()` 返回后 consumer 可能未完全注册
- 需要优雅降级信号——如果 Redis 不可用但 PG 正常，health 应该反映"降级"而非"全部正常"
- 通知延迟不能 > 几秒钟——K8s 的 readiness probe 通常有 3-5 秒容忍窗口

**预期的架构变更：**

```
启动阶段状态机：

     Initializing
         │
         ▼
    ConfigLoaded ────────→ PersistenceReady
         │                       │
         │                       ▼
         │                  ReposReady
         │                       │
         │                       ▼
         │                  ServicesReady
         │                       │
         │                       ▼
         │               ┌─ BusConsumerReady ◄── run_bus_listener 注册完成
         │               │
         │               ▼
         │         BackgroundSpawned
         │               │
         │               ▼
         │     ┌─ BotReady(agent_bot)  ← 每个 bot 独立注册
         │     ├─ BotReady(ooo_bot)
         │     ├─ BotReady(push_bot)
         │     └─ ...（共 7+ bot）
         │               │
         │               ▼
         │         MetricsReady
         │               │
         │               ▼
         │         RetentionReady
         │               │
         │               ▼
         ▼               ▼
       Ready（前面所有阶段完成）
```

**实现方式：**
- `tokio::sync::Notify` 或 `watch` channel 作为就绪信号
- `spawn_bus_listener` 在 consumer 注册成功后发送信号
- `/health/ready` 检查所有就绪信号，返回 "degraded" 状态和具体未就绪组件
- 可配置 `AERO_READY_TIMEOUT_SECS`——超时后未就绪的组件记录 error 但允许服务启动

### 方向四：前端测试基础层（P1，中优先级）

**为什么需要：** ~4300 行 JS 零测试意味着每次前端改动都需要手动回归。43% 的 API 调用静默吞错误意味着生产问题不可追溯。

**核心挑战：**
- Web SPA 没有模块系统（零依赖 ES2020），无法直接使用 `import` 测试
- DOM 模拟 + 异步事件使测试设置复杂
- 测试基础设施的收益是长期的，短期投入看起来"不紧迫"

**预期的架构变更：**
- 不需要引入框架。纯函数测试（`SeqGate`, `api.js` 核心调用）可以用 Node 18+ 原生执行
- 为 `api.js` 中的每个 API 函数增加返回类型/error 信号——当前静默 `.catch(() => {})` 改为至少 `console.error`
- 增量引入测试：不需要一次覆盖全部，从最关键的 `api.js` 开始

### 方向五：灾难恢复 Runbook 体系（P2，低优先级但高价值）

**为什么需要：** 372 份需求分析文档 vs 1 份 runbook——系统投入生产后，运维人员最需要的是"PG 挂了怎么办"而不是"第五版方向分析"。

**核心挑战：**
- 数据跨存储（PG + Redis + NATS + 文件系统），没有备份一致性保障
- 13 个独立的 retention 配置，没有总览矩阵
- GDPR 批量导出没有管理端 API

**预期产物（非代码变更）：**
- 3-5 页关键 runbook：PG 恢复、Redis 重建、NATS stream 重建、全量迁移回滚
- 备份脚本（PG dump + blob tar + 配置归档）
- 数据生命周期总览矩阵文档

---

## 3. 接口设计建议

### 3.1 关键原则

**向后兼容优先。** 当前接口（bot `run(state)` 签名, `RoomEvent` 枚举, hub `fan_out_raw`）不应为新增功能而改变。扩展通过 wrapper/adapter 而非修改核心路径。

**显式依赖 > 隐式环境变量。** 所有现有 `env::var()` 调用应逐步替换为从 `AppState.config` 读取。新模块禁止直接调用 `env::var()`——可以通过 clippy lint 或 `truth-check.sh` 规则强制执行。

**以 traits 而非具体类型定义模块边界。** 例如：`push_gateway` 的 `FakeGateway` 和生产 `FcmGateway` 已经共享 trait——这是好模式，应推广到 blob store、AI provider 等。

### 3.2 建议新增抽象层

**ManagedTask trait：**

```rust
/// 一个可被监控的后台任务。
/// 不是所有任务都需要实现——核心数据平面（bus listener、关键 bot）必须。
trait ManagedTask {
    /// 任务名称（用于日志、指标、健康检查）
    fn name(&self) -> &'static str;
    
    /// 执行入口。cancel 触发时应尽快退出。
    async fn run(self, cancel: CancellationToken) -> Result<(), TaskError>;
    
    /// 可选：任务特定的健康检查（默认检查任务是否还在运行）
    fn health_check(&self) -> HealthStatus { HealthStatus::Alive }
}

struct TaskError {
    kind: ErrorKind,  // Transient | Permanent | Fatal
    message: String,
}
```

**不需要为所有 23 个任务实现——分层推进：**
1. 第一层（P0）：7 个 bot + 2 个 bus listener，需要 CancelToken + 健康信号
2. 第二层（P1）：定时器和 GC 任务，需要健康信号（已有 CancelToken）
3. 第三层（P2）：指标采样等，低风险任务保持现状

### 3.3 /debug/config 接口设计

**为什么需要：** 这是运维可观测性的基础。没有它，运维人员无法知道生效的配置是什么。

**接口约束：**
- 必须由 admin bearer token 或 internal network 保护
- 返回所有可配置项（包括有默认值的），附带来源标注（env var、config.toml、默认值）
- 响应格式不承诺稳定（这是 debug 工具，不是 API 契约）

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

**目前不需要引入新的主要基础设施组件。** 当前技术栈（Rust + tokio + Axum + sqlx + fred + async-nats + str0m）是经过验证的组合，所有架构问题都可以在当前栈内解决。

**可能考虑引入的辅助库：**

| 问题域 | 候选技术 | 评估 |
|--------|---------|------|
| 配置治理 | config crate（Rust）或 figment 更深度使用 | 当前已经使用了 figment，只需要更系统地使用它——非新增依赖 |
| 任务治理 | 自建 Supervisor | 减少外部依赖，当前模式简单不需要复杂框架 |
| 前端测试 | Node 内置 test runner (node:test) 或 vitest | Node 内置 runner 零依赖，适合纯函数测试 |
| 健康仪表盘 | Prometheus + Grafana | 已在用，不需要新增 |

**决策：不引入新的主要框架。** 架构问题主要是模式缺失（lifecycle、observability），不是技术栈不足。

### 4.2 第三方依赖评估标准

当前 `workspace.dependencies` 定义了 16 个内部 crate，外部依赖数量合理。新增依赖应遵循：

1. **必要性**：能否在当前依赖中解决？（例如：是否可以用已引入的 tokio-util 替代新增 crate）
2. **成熟度**：是否维护活跃、Rust 生态兼容
3. **安全影响**：是否引入 unsafe code（当前 workspace 设定了 `unsafe_code = "forbid"`）
4. **编译影响**：是否会显著增加编译时间

### 4.3 自建 vs 采购

| 问题 | 决策依据 | 结论 |
|------|---------|------|
| 后台任务管理 | 模式简单，tokio-util 已提供 TaskTracker + CancellationToken，只需增加 health signal wrapper | 自建 |
| 配置治理 | 已有 figment，只需在其上构建 RuntimeConfig | 自建 |
| AI provider 切换 | 当前已通过 trait 抽象（AiService + AiWorker），支持 Anthropic + Voyage + HashEmbedder | 已解耦，保持 |
| 前端测试 | 纯函数测试，无框架依赖 | 自建 |
| 推送网关 | 已通过 trait 抽象（FCM + APNs + FakeGateway） | 保持 |

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 影响面 | 估计工作量（人·天） |
|--------|------|--------|-------------------|
| P0 | 后台任务治理 | 生产可靠性、数据不丢失 | 5-8 |
| P0 | 配置管理统一 | 运维可操作性、排障速度 | 3-5 |
| P1 | 启动就绪信号 | 启动窗口数据完整性 | 2-3 |
| P1 | 前端测试基础 | 回归安全性、错误可追溯 | 3-5 |
| P2 | 灾难恢复 runbook | 长期运维能力 | 1-2（文档为主） |

### 5.2 阶段划分

**阶段一（1-2 周）：后台任务治理 + 配置管理入口（P0）**

里程碑 1：`supervisor` 模块 + 7 bot + 2 bus listener 增加 CancellationToken
- 修改 `agent_bot`, `ooo_bot`, `push_bot`, `golive_bot`, `transcribe_bot`, `moderation_bot`, `unfurl_bot` 的 `run()` 签名，增加 `cancel: CancellationToken`
- 修改 `run_bus_listener`, `run_live_bus_listener` 类似
- 在 `background.rs` 中增加 `spawn_managed` wrapper
- 每周期的健康检查集成到 `probe_deps`

里程碑 2：`AppState.config` + `/debug/config` 端点
- 新增 `RuntimeConfig` struct（收集当前分散的 env var）
- `AppState` 持有 `Arc<RuntimeConfig>`
- 配置读取从 `env::var()` 迁移到 `state.config.X`（每个模块逐步迁移）
- 新增 `/debug/config` 端点

**阶段二（1-2 周）：启动就绪信号 + 前端测试（P1）**

里程碑 3：启动就绪信号通道
- 新增 `ReadinessRegistry`，`spawn_bus_listener` 注册就绪信号
- `/health/ready` 增加组件级状态
- 可配置的启动超时

里程碑 4：前端测试基础
- `api.js` 核心调用的单元测试（Node test runner）
- `.catch(() => {})` 改为至少 `console.error`
- 增量——不追求覆盖，追求关键路径

**阶段三（P2，1-3 天）：runbook + 数据总览**

里程碑 5：
- 3 份关键 runbook（PG 恢复、Redis 重建、NATS stream 重建）
- 数据生命周期总览矩阵
- 备份脚本

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 为 7 bot 加 CancelToken 时引入回归 | 中 | 低 | 每个 bot 有独立 entry point，修改签名后编译期捕获 |
| 重构配置时遗漏 env::var() 调用点 | 高 | 中 | `truth-check.sh` 增强，不全部迁移完也可以——逐步迁移 |
| 前端测试基础设施投入后 ROI 不明显 | 中 | 低 | 从最关键路径开始，不一次性投入大量精力 |
| 启动就绪信号增加启动延迟 | 低 | 低 | 默认用 Notify（零成本），仅等待核心组件 |
| 架构改进与功能开发资源竞争 | 高 | 高 | 分阶段推进，每个阶段有明确的停止条件（OKR） |

### 5.4 停止条件（什么时候可以认为方向已完成）

| 方向 | 完成标准 |
|------|---------|
| 后台任务治理 | 所有 9 个核心任务（7 bot + 2 bus listener）支持 CancelToken；/health/ready 包含后台任务状态；有 1 个告警规则检测 dead task |
| 配置管理统一 | 所有 boot/ 模块中新增的配置必须通过 state.config 读取；存量迁移 >80%；/debug/config 返回生效配置 |
| 启动就绪信号 | start-up window 的 NATS consumer 未就绪状态下 /health/ready 返回 "degraded"；启动时序在文档中明确 |
| 前端测试基础 | 最关键的 3 个 api 函数有测试；无静默 `.catch(() => {})` |
| 灾难恢复 | 3 份 runbook；备份脚本；数据生命周期文档 |

---

## 总结

Aero IM 的后端架构在技术选择上是正确的（事件驱动 + NATS + Redis sorted-set + bounded fan-out），boot 模块分解展现了良好的工程纪律。但系统存在两个 P0 的架构空洞——**后台任务的生命周期管理**和**配置管理的去中心化**——恰好是生产系统最容易静默失败的两个维度。

好消息是：这两个问题都可以在当前技术栈内解决，不需要引入新的基础设施组件。坏消息是：它们需要架构层面的纪律变更而不仅仅是代码修补——需要将"所有任务都有生命周期"和"所有配置都有唯一来源"提升为架构规范，而不仅仅是编码习惯。

建议从后台任务治理切入——它不仅修复了最危险的生产漏洞（HTTP 200 但 bot 全部死掉），还能在实行过程中建立 supervisor 模式，为后续的健康检查和优雅关闭提供统一框架。
