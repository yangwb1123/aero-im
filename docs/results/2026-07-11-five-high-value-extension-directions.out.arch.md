# 架构分析报告：Aero IM 系统性技术债与架构演进

## 1. 架构评估

### 1.1 当前架构的优势

**事件驱动骨架成熟度高。** 项目采用的「NATS JetStream 作为跨实例事实源 → 进程内 Hub 扇出 → WebSocket」的架构模式，在 IM+直播场景下是正确的选择。这一模式的优势在于：

- **无状态水平扩展**：Hub 只做进程内 bounded mpsc 扇出，多开实例即水平扩，无需分布式协调
- **两类 consumer 设计恰当**：`im.room.*` 用 durable consumer（at-least-once，消息不可丢），`live.stream.*` 用 ephemeral（弹幕可丢，每实例必须见到所有事件）。这是成本与可靠性之间的合理权衡
- **Per-subject 单调 seq**：`bus/seq.rs` 注入序列号，为客户端去重/排序提供基础设施——这条被大多数 IM 系统忽略，但在跨实例重投场景下是必要的

**分层清晰且依赖方向正确。** crate 自下而上（common → bus/storage/auth → im-core → server）无成环依赖，每个 crate 有明确定义的职责边界。这是 Rust 项目中为数不多的「按领域切 crate 且切对了」的例子。

**幂等与失败处理模式已经体系化。** 多个 bot/worker 共享类似模式（`ON CONFLICT DO NOTHING`、`SKIP LOCKED`、预算门控、fail-open 降级），说明团队已经形成了可复用的设计模式——虽然未提取为公共库。

### 1.2 关键架构债务

以下债务按严重程度排列：

#### P0：缺少运行时依赖断路器和健康管理层（方向二的实质问题）

当前架构对下游依赖（PG、Redis、NATS）的健康管理完全依赖各客户端的内部重试策略。这意味着：

- **无统一降级策略**：Redis 故障时，有的是 fail-open（限流），有的可能直接 500
- **缺乏启动门控**：`connect_with_retry` 只在 boot 时使用，运行时断连后各客户端独自重连，无 grace period、无半开检测
- **级联故障风险**：一个依赖的抖动可能拖死整个请求管道（无超时传播、无舱壁隔离）

**具体风险场景**：Redis 主从切换导致 3s 不可用→ 在途请求都在阻塞等待→ tokio 线程池占满→ 即使 PG/NATS 正常，新请求也无法被处理。这是典型的缺乏舱壁隔离。

#### P0：JWT 密钥架构未完成（方向一的实质问题）

验证文档指出了关键点：源码已有 `verify_keys: HashMap<String, DecodingKey>`（`jwt.rs:45`），说明作者**意识到需要多密钥支持**，但生产部署仍使用单密钥：

- 无 JWKS 端点暴露
- 无 `kid` 头注入
- 密钥轮换需要滚动重启所有服务实例

这意味着密钥泄露后恢复时间按小时计算（滚动重启），而非按秒计算（轮换 JWKS 缓存）。在 30K DAU 规模下，每次强制重启对用户感知的影响放大。

#### P1：缺少写路径的全局一致性视图

当前的幂等模式是每个 bot/worker 各自为政（`ON CONFLICT DO NOTHING`、`defer`、预算门控等）。缺少一个**中心化的事务边界管理器**：

- 没有跨 domain 的 saga 模式
- 没有分布式的幂等键框架
- 部分定时器（如 `retention_sweep`）在单进程执行，多实例下需要分布式锁

随着 bot/worker 数量增长（目前已有 7 个 bot + 多个 worker/timer），各自为政的幂等策略会变成维护负担。

#### P2：配置体系分散

双下划线环境变量（`AERO__SECTION__KEY`）与单下划线例外（`AERO_RATE_LIMIT_PER_SEC`、`AERO_S3_*` 等）共存，且分布在 figment + 散落的 `std::env::var` 调用中。缺少一个注册表机制，新的 configuration knob 要么加在 config.example.toml 里被忽略，要么藏在代码深处。

---

## 2. 核心扩展方向

### 方向一：运行时依赖健康管理层（Circuit Breaker + Health Manager）

**为什么需要：** 这是 P0 运营缺口。当前系统对下游依赖的故障无统一应对策略。当系统扩展至 30K DAU、跨三个可用区部署时，依赖故障不再是「如果」而是「何时」。

**核心挑战：**

1. **Rust 的 trait 系统与异步重试的融合**：需要给 `PgPool`、`RedisClient`、`NatsClient` 包裹一层健康 proxy，暴露统一 `HealthCheck` trait
2. **优雅的降级语义**：不同业务对同一个依赖的故障容忍度不同（限流 fail-open vs 消息发送 fail-close），降级策略需要与业务上下文绑定
3. **启动门控的精确度**：liveness vs readiness 探针应反映真实的状态——PG 连接池耗尽时 readiness 应该返回 503，但 liveness 不应触发 pod 重启

**预期的架构变更：**

```
Current:
  Route → PgPool (直接使用)
  Route → RedisClient (直接使用)
  Route → NatsClient (直接使用)

Proposed:
  Route → HealthManager
            ├── PgHealthProxy { inner: PgPool, circuit: CircuitBreaker }
            ├── RedisHealthProxy { inner: RedisClient, circuit: CircuitBreaker }
            └── NatsHealthProxy { inner: NatsClient, circuit: CircuitBreaker }
```

**选项 A（轻量级）**：在 `aero-common` 中添加 `health` 模块，提供 `CircuitBreaker` 结构和 `HealthRegistry`。每个客户端在其上注册。`/health/ready` 路由查询注册表。实现成本约 3-5 天。

**选项 B（全面型）**：引入 `tokio- graceful-shutdown` + tonic gRPC health proto。在 `aero-server` 中起 gRPC health 服务，K8s probe 直接指向。同时暴露 Prometheus `up{依赖名}` 指标。实现成本约 7-10 天。

**权衡**：选项 A 与当前代码风格一致（纯内建，无新依赖），但缺少 gRPC health prober 的生态集成。选项 B 更标准但引入了 gRPC 依赖。推荐选项 A，因为当前部署模型不依赖 K8s——gRPC health 短期内无消费方。

### 方向二：JWKS + 多密钥轮换

**为什么需要：** P0 安全缺口。见 1.2 节分析。重要的是现有的 `verify_keys: HashMap` 结构降低了实现成本。

**核心挑战：**

1. **JWKS 端点的保护**：JWKS 本身是公开信息，但响应不可太大（多密钥场景）。需要缓存 + `Cache-Control: max-age=86400`
2. **密钥生命周期管理**：active → decommissioned → removed 三个阶段。过渡期需要 `iat`（issued-at）校验防止旧签名被无限期接受
3. **Stale key 泄漏风险**：如果 token 用已移除的密钥签名，只有在 token 过期时才会被拒绝——需要在 jwt middleware 中添加 `nbf`（not-before）校验

**预期的架构变更：**

- `aero-auth/src/jwks.rs`：JWKS 端点 handler + 缓存层
- `aero-auth/src/jwt.rs`：`decode_jwt` 从单密钥改为遍历 `verify_keys` + 匹配 `kid`
- `migrations/`：如果密钥持久化到 PG（替代 env），需要 `signing_keys` 表
- 管理端点：`POST /api/admin/keys/rotate` 触发新密钥生成

**实施路线：**

| 阶段 | 内容 | 风险 |
|------|------|------|
| 1 | `verify_keys` HashMap 接入 `kid` 匹配 | 无，已存在结构 |
| 2 | 管理 API 生成/激活/吊销密钥 | 需要鉴权守卫 |
| 3 | 可选：PG 持久化 + 自动轮换 timer | 密钥存储安全 |

### 方向三：统一幂等框架（Deduplication + Idempotency Layer）

**为什么需要：** 当前 7 个 bot + 多个 worker/timer 各自实现幂等逻辑。当新增 bot 时，开发者需要重新实现相同的模式。统一的幂等框架可以：

- 减少新增 bot 的开发成本（从 1 天降至 2 小时）
- 在 at-least-once 语义下提供 exactly-once 投递保证（对支付/计费场景至关重要）
- 提供观测指标（幂等命中率、重复投递来源）

**核心挑战：**

1. **幂等键的生成策略**：需要跨 subject 的唯一性（message_id + bot_kind + 重试次数？）。过度抽象会丢失业务语义
2. **存储后端选择**：Redis（高性能、有 TTL 自动过期）vs PG（事务性、可审计）。对于需要事务性幂等的场景（如 moderation 的软删+审计需在同一事务），PG 更合适
3. **GC 策略**：幂等键不能永远保留。需要 TTL + 清扫。Redis 的 TTL 自动过期是最简单的方案

**架构提案：**

```rust
// 核心 trait
#[async_trait]
pub trait IdempotentHandler: Send + Sync {
    type Event: DeserializeOwned + IdempotencyKey;
    type Output: Send;

    async fn handle(&self, event: Self::Event) -> Result<Self::Output>;
    async fn idempotency_store(&self) -> &dyn IdempotencyStore;
}

// 运行时
pub struct IdempotentRunner<H: IdempotentHandler> {
    inner: H,
    metrics: IdempotencyMetrics,
}

impl<H: IdempotentHandler> IdempotentRunner<H> {
    pub async fn run(&self, event: H::Event) -> Result<H::Output> {
        let key = event.idempotency_key();
        if self.inner.idempotency_store().exists(&key).await? {
            self.metrics.hit.inc();
            return self.inner.idempotency_store().get(&key).await?;
        }
        let output = self.inner.handle(event).await?;
        self.inner.idempotency_store().set(&key, &output).await?;
        self.metrics.miss.inc();
        Ok(output)
    }
}
```

**重要的是**，这个抽象不应该强制所有 handler 使用——对于 ooo_bot 这种低成本幂等场景，`ON CONFLICT DO NOTHING` 已经足够。幂等框架应该是一个可选组建，不是强制约束。

### 方向四：配置注册表（Config Registry）

**为什么需要：** 当前配置源的分散（figment env + 散落 `std::env::var` + `config.toml`）使得「这个配置在哪定义」成为一个需要全局 grep 才能回答的问题。当系统增长到 50+ configuration knobs 时，需要中心化的定义和校验。

**核心挑战：**

1. **figment 的抽象**：figment 已经提供了不错的来源叠加（env → file → default），但缺少 schema 定义。需要在不引入 `serde::Deserialize` 膨胀的前提下提供「配置项名称 + 类型 + 默认值 + 描述」的注册
2. **环境变量命名一致化**：需要消除单下划线例外（`AERO_RATE_LIMIT_PER_SEC`），全部统一为双下划线（`AERO__RATE_LIMIT__PER_SEC`），但要向后兼容

**架构提案（轻量级）：**

```rust
// 在 aero-common 中
pub struct ConfigKey<T: FromStr> {
    pub name: &'static str,     // 双下划线形式
    pub default: T,
    pub description: &'static str,
    pub deprecated_name: Option<&'static str>, // 向后兼容
}

// 宏
config_key!(AERO__RATE_LIMIT__PER_SEC, u32, 100, "Per-second rate limit per workspace");
// 生成:
// - figment key 解析
// - env override
// - Prometheus info metric
```

这个方向的可选方案是引入 `figment::Providers` 特性，但 figment 的 API 已经被 `Env::prefixed("AERO__").split("__")` 封装了，加入 config registry 不会破坏现有模式。

### 方向五：事务性 Saga 编排层（仅针对多步骤跨域写操作）

**为什么需要：** 当前系统在处理「发送消息 → 审核 → 推送 → 持久化」这样的多步骤流程时，每一步的失败处理是隐式的（要么 fail-open 日志 skip，要么 defer 到下一轮）。当引入付费功能（如打赏确认、订阅计费）时，需要 `Saga` 或 `Outbox` 模式保证最终一致性。

**核心挑战：**

1. **Rust 的类型系统与动态补偿**：Saga 模式需要动态的补偿操作注册。在 Rust 的静态类型系统下，需要 `Box<dyn FnOnce() -> Future>` 或 enum dispatch。静态 dispatch 更安全但扩展性差
2. **跨 NATS subject 的事务边界**：一条消息的「发送 → 审核命中 → 删除」跨越了 `im.room.*` 和内部 API 调用，没有全局事务上下文。需要将 trace ID 作为 saga 上下文传播
3. **与现有幂等框架的关系**：Saga 建立于幂等框架之上（每个步骤可重试），而不是替代它

**注意：当前阶段不建议实施。** 除非引入付费订阅或金融级功能（打赏结算、订阅扣费），否则现有 fail-open 模式 + 幂等键已经足够。这个方向列为 **P2 观察清单** 即可。

---

## 3. 接口设计建议

### 3.1 关键抽象层的接口设计

**Health Proxy trait（最高优先级）：**

```rust
#[async_trait]
pub trait HealthChecked: Send + Sync {
    /// 健康检查，返回状态和可选的诊断信息
    async fn check(&self) -> Result<HealthStatus, HealthError>;
    
    /// 依赖类型标签，用于指标和日志
    fn dependency_type(&self) -> DependencyType; // enum { Database, Cache, MessageQueue, BlobStore }
    
    /// 依赖名称（实例级，如 "pg-primary"、"redis-session"）
    fn dependency_name(&self) -> &'static str;
}

pub struct HealthRegistry {
    checkables: Vec<Box<dyn HealthChecked>>,
    // readiness gate
    ready: Arc<AtomicBool>,
}

impl HealthRegistry {
    pub fn register(&self, checkable: impl HealthChecked + 'static);
    pub fn readiness(&self) -> impl HealthCheckEndpoint;
    pub fn liveness(&self) -> impl HealthCheckEndpoint;
}
```

**关键设计决策：**

- **不加 `fn priority()`**：健康检查不应该有优先级，所有依赖都必须健康才 readiness=true。但 liveness 可以放宽——仅检查是否有致命故障（如与 NATS 的连接完全丢失）。
- **不加 `fn degraded()`**：降级策略不在健康管理层耦合。降级是业务层的关注点——`ImService` 知道 Redis 降级时该 fail-open 还是 fail-close，`HealthRegistry` 不知道也不应该知道。

### 3.2 幂等 Handler 接口设计

幂等抽象的关键决策是**接口的粒度**：

- **宽接口**（如上面的 trait，要求 `type Event: IdempotencyKey`）：类型安全，但与现有 bot 结构不一定兼容（现有 bot 使用 `RoomEvent`，其中 `IdempotencyKey` 可能是 `(room_id, message_id, bot_kind)` 元组）
- **窄接口**（闭包式）：`fn run<E, F>(event: E, handler: F, store: &IdempotencyStore)`——更灵活但丢失类型约束

**建议**：宽接口 + 一个 `impl_idempotent_handler!` 宏，自动为遵循「event → db write → bus publish」模式的 handler 生成实现。对于有特殊幂等语义的 handler（如 `ooo_bot` 的 `ON CONFLICT DO NOTHING`），提供 `#[idempotent(bypass)]` 标记跳过框架。

### 3.3 向后兼容策略

**配置命名**：引入新配置名时，旧名仍然在 `deprecated_name` 字段注册，且在启动时打印 deprecation warning。保留至少一个大版本周期。

**幂等键格式**：如果引入全局幂等框架，幂等键的第一个版本应该使用与现有 bot 相同的键格式（room_id:seq:bot_kind），然后在新版中逐步演进。

**健康端点**：`/health` 现有的响应格式不变（JSON `{status: "ok"}`），新信息（依赖级健康详情）加在 `{"dependencies": [{"name": "pg", "status": "ok"}, ...]}` 字段中，旧客户端忽略未知字段。

---

## 4. 技术选型

### 4.1 需要引入的新依赖

| 组件 | 推荐 | 备选 | 评估结论 |
|------|------|------|----------|
| Circuit Breaker | 自建（~300 行） | `failsafe` crate | 自建。断路器核心逻辑不到 200 行，用已有 `std::time::Instant` + `AtomicUsize` + tokio `Notify` 即可。引入 `failsafe` 需要处理其 0.5 版 API 不稳定问题 |
| JWKS 端点 | `jsonwebtoken` 已有，只需要补 JWKS 序列化 | 引入 `jwk-rs` crate | 不引入。`jsonwebtoken::DecodingKey` 已经有 `from_rsa_components` 和 `from_ec_components`。JWKS 响应可以直接用 `serde_json::json!` 手拼——密钥数量 ≤10，不值得引入依赖 |
| 幂等存储 | Redis（用现有 `fred` 连接池） | PG `idempotency_keys` 表 | 推荐 Redis：TTL 自动过期、低延迟、与现有 presence/roster 共享连接池。PG 方案适合需要事务性幂等的场景（如 moderation 的软删+审计） |

### 4.2 自建 vs 采购的决策框架

对于这个规模的项目（30K DAU，自部署），以下框架适用：

| 判断维度 | 自建条件 | 采购/引入条件 |
|----------|----------|---------------|
| 核心逻辑行数 | < 500 行 | > 2000 行 |
| 与现有模型的匹配度 | 与 crate 边界一致 | 需要适配层桥接不同范式 |
| 升级风险 | 无（完全内部） | API 变更需全局审计 |
| 社区维护 | N/A | 活跃度：commit 间隔 < 30 天 |

**具体判断**：

- **Circuit Breaker**：自建（约 200 行核心逻辑，与 `HealthRegistry` 天然整合）
- **JWKS 轮换**：自建（现有的 `verify_keys` HashMap 结构说明团队已经有设计，只是未完成）
- **幂等框架**：自建（核心是抽象提取，非新功能——已有多个实现可归纳）
- **配置注册表**：自建（宏 + 少量运行时逻辑，约 300 行）
- **OpenTelemetry SDK**：引入（`opentelemetry` crate，现有 `otlp` pipeline 的基础上加依赖级 span）

### 4.3 应该避免的技术栈

- **gRPC**：当前项目是 HTTP+WS 的 Axum + NATS 架构。引入 gRPC 意味着引入 tonic、protobuf 编译链、gRPC health prober。对于健康检查这一个用例，HTTP JSON probe 完全足够。引入 gRPC 的成本（编译时间 + 配置复杂度）大于收益。
- **etcd / ZooKeeper**：当前系统的分布式协调需求非常弱（跨节点 bridge 用 Redis 就够了）。不要引入有状态集群协调器。
- **Kafka**：NATS JetStream 已经是轻量级消息总线。Kafka 的吞吐量优势在 30K DAU 下无意义，且增加运维复杂度。如果未来需要 > 100K 并发消息/秒，才考虑。

---

## 5. 实施路线图

### 优先级矩阵

| 方向 | 影响面 | 实现成本 | 风险降低 | 优先级 |
|------|--------|----------|----------|--------|
| 一、健康管理层 | 运营 | 3-5 天 | 高（P0 运营缺口） | **P0** |
| 二、JWKS 轮换 | 安全 | 2-3 天 | 高（P0 安全缺口） | **P0** |
| 三、幂等框架 | 工程效率 | 5-7 天 | 中 | P1 |
| 四、配置注册表 | 工程效率 | 2-3 天 | 低 | P2 |
| 五、Saga 编排 | 功能 | 7-10 天 | 中（未来需要） | P2 |

### 阶段划分

**阶段一：安全与运营基线（P0，2 周）**

目标：消除当前系统中最容易被攻击或故障放大的两个缺口。

```
Week 1: 健康管理层
  - Day 1-2: HealthChecked trait + HealthRegistry 定义
  - Day 3-4: PG/Redis/NATS 的 HealthProxy 实现
  - Day 5:   /health/ready 路由 + 降级逻辑接入

Week 2: JWKS 轮换
  - Day 1:   verify_keys HashMap 接入 kid 匹配
  - Day 2-3: POST /api/admin/keys/rotate + 密钥生命周期管理
  - Day 4-5: 管理 UI（可选） + 文档更新
```

**风险点**：
- JWKS 端点如果在轮换期间返回过期密钥，所有客户端会瞬间 fail auth。缓解：新密钥激活后保留旧密钥至少 2x token TTL。
- 健康管理层如果错误报告死依赖，会导致 readiness probe 触发 pod 驱逐。缓解：引入 `min_samples` 参数（至少 3 次连续失败才标记 unhealth）。

**阶段二：工程效率提升（P1，1-2 周）**

目标：降低新增 bot/worker 的成本。

```
Week 3-4: 幂等框架
  - 从现有 bot（ooo_bot、moderation_bot）提取幂等模式
  - 设计 IdempotentHandler trait
  - 迁移第一个 bot 作为验证
  - 为其他 bot 提供迁移指南（非强制）
```

**风险点**：
- 过度抽象导致框架与现有个性化幂等逻辑冲突。缓解：保持框架可选，允许 `impl_idempotent_handler!` bypass 标记。
- 幂等存储的 Redis 使用量增长。缓解：TTL 设为 7 天（与 message retention 对齐）。

**阶段三：基础设施成熟（P2，按需）**

```
配置注册表（2-3 天）
  - 提取所有配置点到 ConfigKey registry
  - 统一环境变量命名（双下划线一致性）
  - 启动时校验（missing required key → 启动失败）

Saga 编排（7-10 天，仅在需要付费功能时启动）
  - Saga trait + 补偿步骤注册表
  - 与幂等框架的集成
  - 第一阶段使用场景：打赏 → 余额扣减 → 广播
```

### 关键里程碑

| 里程碑 | 时间 | 可交付物 | 验收标准 |
|--------|------|----------|----------|
| M1: 健康管理层上线 | 阶段一结束 | HealthRegistry + 三个 proxy + readiness 端点 | `curl /health/ready` 在依赖故障时返回 503 |
| M2: 密钥可轮换 | 阶段一结束 | 管理 API 触发轮换 + 客户端无感 | 轮换期间 0 次无效 token 错误 |
| M3: 第一个幂等 handler 迁移 | 阶段二结束 | ooo_bot 或 moderation_bot 使用框架 | bot 行为不变，幂等率指标可见 |
| M4: 配置统一 | 阶段三 | 所有配置点可通过 env override | `grep -r "env::var" src/` 为 0 |

### 长期架构 Vision

```
┌──────────────────────────────────────────────────┐
│                   API Gateway                     │
│  (Axum, HTTP/WS, WHIP/WHEP, HLS, Metrics, Health)│
└────────┬────────────┬──────────────┬──────────────┘
         │            │              │
┌────────▼──┐  ┌──────▼──────┐  ┌───▼────────────┐
│  Auth     │  │  ImService  │  │  Live Service  │
│  (JWT+    │  │  (Msg,Call, │  │  (Ingest,SFU,  │
│   JWKS)   │  │   AI,RBAC)  │  │   Chat,Gift)   │
└────────┬──┘  └──────┬──────┘  └───┬────────────┘
         │            │              │
         └────────────┼──────────────┘
                      │
         ┌────────────▼──────────────┐
         │   HealthManager           │
         │   (CircuitBreaker +       │
         │    HealthRegistry)        │
         └──┬─────────┬─────────┬────┘
            │         │         │
    ┌───────▼──┐ ┌───▼────┐ ┌──▼──────┐
    │ Postgres │ │ Redis  │ │ NATS    │
    │ (PgPool) │ │(fred)  │ │(async-  │
    │         │ │        │ │ nats)  │
    └─────────┘ └────────┘ └─────────┘
```

健康管理层位于 Gateway 和 Service 之间，成为所有对外请求的必经之路。幂等框架内嵌在 bot/worker 层（不阻塞主请求路径）。JWKS 集成在 Auth 层。Saga 编排位于 Service 层之上，仅在需要跨域事务时启用。

---

## 总结

Aero IM 的架构基础（事件驱动、消息总线、分层 crate）是扎实的。当前的主要债务不是架构方向错误，而是**基础设施层不完整**——健康管理（P0）、密钥管理（P0）、幂等抽象（P1）这三个组件在代码中有散落的实现，但没有被提取为共享基础设施。

这恰好意味着修复成本相对较低：核心逻辑已经存在（`connect_with_retry` 的退避逻辑、`verify_keys` 的多密钥结构、各个 bot 的幂等模式），需要的是抽象和统一，而非重写。

建议的顺序：先做 P0（2 周）——这两个方向解决的是「系统能不能在运维事故中存活」的问题。然后评估 P1（1-2 周）——是否值得投入取决于团队新增 bot 的频率。P2 进入观察清单，仅在业务需求驱动时实施。
