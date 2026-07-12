# 架构分析报告：Aero IM 高价值扩展方向

> **分析依据**: `docs/requirements/2026-07-11-code-scan-high-value-extensions.md`  
> **视角**: 系统架构评估与技术方案推演  
> **背景**: 全量扫描 16 crate / 157 SQL 迁移 / ~46K Rust / ~5.9K Web SPA / 126 路由模块后识别出的 5 个未被系统性覆盖的架构缺口

---

## 一、架构评估

### 1.1 当前架构的核心优势

该项目的架构选择有其鲜明的设计哲学，值得先予肯定：

**事件驱动骨架成熟**。NATS JetStream 作为跨实例事实源 + 进程内 Hub 的 bounded mpsc 扇出，构成了可靠的水平扩展骨架。durable consumer + at-least-once 保证 + per-subject 单调 seq 的组合，在「可靠投递」和「避免全局状态」之间取得了务实的平衡。两个 consumer 策略的选择更是见功底——`im.room.*` 用 durable（不能丢消息），`live.stream.*` 用 ephemeral（丢几条弹幕无碍）。

**组件隔离遵从 crate 边界**。从 `aero-common` 到 `aero-server` 的自下而上依赖树，整体设计合理。关键收益是每个 crate 可独立演进，编译隔离，且新领域（如 AI、Push）作为独立 crate 引入时不扰动核心 IM 逻辑。跨 crate 的接口由 `common` 中的共享类型所定义，不产生循环依赖。

**降级路径的潜在意识已存在**。源代码中散布着 fail-open 设计（`transcribe_bot` 的 stub 占位、`unfurl_bot` 的 log-skip、`moderation_bot` 的 skip 而非背压），说明团队有韧性意识，只是尚未系统化。

### 1.2 结构性局限

**韧性策略是点状而非系统性**。当前 fail-open 模式是每个 bot 各自实现的局部策略（各自的 try/catch + log）或完全不存在——没有统一的断路器模式、没有健康依赖声明、没有全局降级状态机。后果是：Redis 宕机后，一个 presence 查询失败会 500 掉整个请求，即使该请求核心操作（消息存储）完全不依赖 Redis。这是**故障隔离边界的缺失**，也是本文方向五的核心关切。

**协议层存在「半拉桥」模式**。`Interaction`、`MessageSeen`、`Welcome` 的帧定义、总线 event、服务端发送全链路就位，唯独客户端没有 handler。这种现象暗示：架构师在协议设计层做好了扩展预留，但产品优先级排序导致这些能力从未被端到端验证。这产生了隐蔽的技术债——这些代码从未在生产流量中经过考验，接收帧无 handler 意味着静默丢帧，开发者难以发现。从协议完整性角度看，这是**发送端与接收端的状态不一致**。

**关键安全凭据使用静态明文**。这是当前架构中最紧急的工程债务——TURN 凭据在启动时从环境变量读取一次并永久有效。对比主流实践（HMAC 短时效签名 + 每会话唯一的临时凭据），静态模式的风险跨越代码质量、安全合规到运维可用性三个层面。这不是功能缺失而是**安全基线的缺位**。

**广播成本模型未设上限**。`@everyone` 的成员展开没有成员数门控、没有发送频率限制、没有 per-workspace 开关。系统对写放大的防护完全依赖通知侧的 `NotifyBatch` 优化（从 O(N) NATS publish 降至 1），但 DB 写入和推送仍是 O(N)。这是**缺乏上行限流机制的典型表现**——一个失陷账户的单条消息即可产生万级别的 DB 写入。

### 1.3 架构债务特征

总结存量债务的四种形态：

| 债务类型 | 实例 | 偿债成本曲线 |
|----------|------|------------|
| **安全债务** | TURN 静态凭据、PAT 无轮换 | 当前低（几天），越晚越高（生产环境泄露后不可逆） |
| **韧性债务** | 无断路器、无降级策略 | 线性增长（每新增一个 bot/worker 都在延续无防护模式） |
| **协议债务** | Interaction/MessageSeen/ delivery_cursors | 已处于最低点（基础设施就位，只差客户端接入） |
| **限流债务** | @everyone 无门控 | 当前低（大群尚少），群规模增长后急剧恶化 |

---

## 二、扩展方向分析

本文档识别出 5 个方向。我将其按架构深度重新组织——**不是按优先级，而是按对系统架构的影响深度**：

### 方向 A：韧性策略系统化（深度：大）

**对应文档方向五，但范围更广。**

#### 为什么需要

韧性不是功能特性而是系统的非功能属性（NFPA）。在当前架构中，每一个基础设施依赖（PG、Redis、NATS、AI API）的故障都会造成级联效应——即使故障不影响该请求的核心操作。更危险的是，随着 bot/worker 数量持续增加（agent_bot、ooo_bot、unfurl_bot、transcribe_bot、golive_bot、push_bot、moderation_bot……），「每个 bot 各自写 try/catch/log」的模式不可扩展。

**一句话**: 缺的不是降级代码，而是降级框架。

#### 核心挑战

1. **依赖关系图显式化**。目前没有全局声明「端点 X 依赖资源 Y，Y 宕机时 X 可降级模式 M」。需要为每个 handler 做依赖分析——这在已有 120+ 路由模块的规模下是繁重工作。
2. **降级行为可观测化**。降级不能无声无息。当前所有降级（如 `transcribe_bot` 的 stub 占位）是静默发生的，运维无法知道系统当前处于降级状态。
3. **恢复逻辑非幂等**。降级后恢复时需要有「退出降级模式」的明确路径——如果只是重新轮询 Redis/NATS 的健康状态，需要有足够的等待时间避免抖动。

#### 架构变更

```
当前：每个 handler/worker 自行判断故障
目标：健康注册器 + 断路器 + 降级策略表

┌─────────────────────────────────────────┐
│  HealthRegistry                          │
│  ├── redis: CircuitState{Closed/Open/…}  │
│  ├── nats:  CircuitState{…}              │
│  ├── ai:    CircuitState{…}              │
│  └── pg:    CircuitState{…}              │
├─────────────────────────────────────────┤
│  handler 查询:                            │
│  health.redis().degraded_mode()           │
│    → None（正常）/PresenceStale（降级）   │
└─────────────────────────────────────────┘
```

核心新增：
- `HealthRegistry`：全局单例，统一维护各基础设施的健康状态和断路器状态
- `DegradedMode` enum：每个依赖描述当前降级行为（如 `Redis: PresenceStale` / `Nats: EventBatching` / `Ai: Throttled(60s)`）
- `/health/degraded` 端点：聚合当前降级模式，运维可集成到告警
- WS 帧 `degraded`：通知客户端当前降级状态（UI 显示黄色横幅）

#### 对现有系统的影响

- **增量改造**：已有 fail-open 的 bot 保留逻辑但切换到通过 `HealthRegistry` 查询
- **无 fail-open 的路径**（如 presence 查询、NATS publish）：需逐点评估可接受的降级行为
- **风险**：过度降级可能隐瞒真实故障——降级策略必须是**保守的**（宁可 500 也不要让用户以为消息已发送但实际未投递）

---

### 方向 B：安全凭据注入层（深度：中）

**对应文档方向一，但与方向五的韧性策略有交集。**

#### 为什么需要

安全凭据（TURN、AI API key、JWT secret、internal bridge secret）目前散布在环境变量、启动配置和运行时内存中。系统的凭据管理没有统一的抽象层——每个组件各自 `std::env::var` 或从 config 读取一次后永久持有。这意味着：

- 凭据无法在不重启服务的情况下轮换
- 无法 audit「谁在何时使用了什么凭据」
- 新凭据类型的引入需要开发者自己实现读取/缓存/轮换逻辑

#### 核心挑战

1. **性能和可用性的权衡**。如果引入集中式凭据存储（如 Vault/ AWS Secrets Manager），每个凭据读取都增加一次网络往返和延迟。纯本地轮换（内存 + TTL re-read）更轻量，但失去了集中审计和吊销能力。
2. **凭据吊销的传播延迟**。如果服务端吊销了某个用户的 TURN 凭据，正在进行的 WebRTC 通话如何处理？直接断开？等通话自然结束？——这个问题没有标准答案。

#### 架构变更

层次不要太多——只需要三个抽象：

```
trait CredentialStore: Send + Sync {
    /// 获取指定凭据，含 TTL 缓存
    async fn get(&self, key: CredentialKey) -> Result<CredentialValue, CredentialError>;
    /// 手动吊销缓存中的凭据（下次 get 重新读取）
    async fn revoke(&self, key: CredentialKey) -> Result<()>;
}
```

实现分两层：
- **本地实现**：从 `AERO__*` env 读取，内存缓存（`tokio::sync::watch` 做热更新），TLS config file watch 做自动轮换
- **外部实现**：对接 Vault / AWS Secrets Manager / Redis（可选，非必须）

TURN 凭据从 `CredentialStore::get(CredentialKey::TurnSecret)` 读取，每次 `/api/rtc-config` 请求用 HMAC-SHA1 签名生成短时效凭据。

#### 对现有系统的影响

- **小**：只影响 `rtc_config_payload()` 函数的实现。其他凭据（AI keys 等）可逐步迁移，不影响现有功能。

---

### 方向 C：广播成本治理（深度：浅）

**对应文档方向二。**

#### 为什么需要

广播机制的 DoS 防护不是功能特性而是运维必需品。当前系统对 `@everyone` 的展开没有任何防御——没有成员数门控、没有频率限制、没有管理员覆盖。在万人大群中，一条 `@everyone` 消息会产生与群成员数等量的 notification 行写入 + 推送尝试。

更微妙的风险：当前 `NotifyBatch` 已经将 O(N) NATS publish 优化为 1 个 batch 发布，但 **DB 写放大仍未被解决**——`notifications` 表的一行插入对应每个目标成员。如果 notification 写入本身实施批量化（`INSERT INTO notifications (...) SELECT ... FROM unnest($1)`），可以降低 10 倍以上的 PG 往返。

#### 核心挑战

1. **通知语义一致性**。如果跳过离线成员的 `@everyone` 通知（改用 feed 聚合），产品需要定义「离线多久算错过」——这对部分场景是可接受的折中，但不是所有场景。
2. **广播许可的不可枚举性**。如果只允许 owner/admin 发送 `@everyone`，那一个群组需要一个 admin 在线才能发全群通知——这可能阻塞紧急信息传递。

#### 架构变更

主要是配置 + 限流，不需要结构性架构变更：

```
新增配置:
- AERO_BROADCAST_MEMBER_THRESHOLD (默认 100): 超过此成员数时 @everyone 限制生效
- AERO_BROADCAST_RATE_LIMIT (默认 1/60s): 每个房间每分钟最多 1 条 @everyone

新增校验链:
  detect @everyone token
  → 是否超过成员阈值？
    → 如果超阈值：检查 per-room rate limit（Redis 滑动窗口）
    → 超限则降级：将 @everyone 转为 @here（仅在线成员）
  → 写入通知时走批量化 INSERT
```

#### 对现有系统的影响

- **小**：不影响现有消息流程。rate limit 是新增校验点，fail-open（限流不可用时默认放行以避免误拦截）。
- **注意**：批量化 notification 写入需要修改 `notification_repo::insert_notifications` 的接口签名——从 `iter` 逐行 insert 改为 `Vec` 批量参数。

---

### 方向 D：客户端状态收敛（深度：浅）

**对应文档方向三和四，两者实质上是一个问题的两面：协议数据从服务端流向客户端，但客户端不消费。**

#### 为什么需要

这两个方向的价值在于它们的**投入产出比极高**：基础设施已完成，只差客户端接入。`delivery_cursors` 表 + REST API 是已在生产服役的代码，`Interaction`/`MessageSeen` 帧定义 + 服务端广播也是完整链路——这些不是「需要设计」的能力，而是「忘记接线」的能力。

从产品角度，多设备已读收敛是 IM 类产品的标准期望——用户不会区分「在我的手机上已读」和「在这个设备上已读」是两个不同的状态。一个「Red 5」类的未读计数不一致是直接影响 NPS 的问题。

#### 核心挑战

1. **跨设备已读通知的延迟与顺序**。如果设备 A 已读 msg=100，设备 B 通过 WS 的 `Read` 帧收到通知，但设备 B 尚未收到 msg=100（顺序问题）。这意味着 `_lastSeen` 不能在收到 `Read` 帧时直接推进，需要比对本地消息列表中的最大消息 ID。
2. **`MessageSeen` 的写入代价**。每个用户、每条消息都广播 `MessageSeen` 意味着 O(N_users × N_messages) 的扇出——这在大型房间中不可接受。实际应该：用户每 markRead 一次，广播一次，客户端用 `max(message_seen.message_id)` 聚合显示。
3. **隐私选择**。是否每个用户都可见「谁已读」？是否需要 per-user 的已读隐私开关（类似 Slack 的「已读回执关闭」）？这需要产品决策，且影响实现。

#### 架构变更

不需要引入新的架构抽象。只需：

1. 客户端 `hookWs` 的 `open` 回调中增加 `GET /api/rooms/:id/delivery-cursor` 调用，校准 `_lastSeen`
2. `markRead` 成功后同步 `PUT /api/rooms/:id/delivery-cursor` 写入持久化游标
3. `_lastSeen` 增加 `localStorage` 持久化（页面刷新保护）
4. `ws.on('msg:message_seen', handler)` 注册：更新对应消息的已读指示器
5. `ws.on('msg:interaction', handler)` 注册：更新交互式 Button/Select 的实时反馈

#### 对现有系统的影响

- **极小**：仅影响 `web/` 下的 SPA 代码。零服务端变更。
- **风险**：`MessageSeen` 帧在大型房间的广播频率需要限流——用户每翻页一次触发 markRead，不应广播给全体。建议：只在 `message_id` 大于当前 `_lastSeen` 时广播 `MessageSeen`。

---

## 三、接口设计建议

### 3.1 健康注册器接口

核心设计原则：**声明式的依赖描述，而非命令式的 try/catch**。

```rust
/// 每个 handler 声明它对基础设施的依赖关系
struct HandlerDependency {
    /// 必须强依赖：该依赖故障时 handler 必须拒绝服务
    required: &'static [DependencyKind],
    /// 可降级依赖：该依赖故障时 handler 以降级模式继续
    degradable: &'static [DependencyKind],
}

/// 每个 handler 在注册时附带依赖声明
let route = get("/api/rooms/:id/messages")
    .with_dependencies(HandlerDependency {
        required: &[PG],
        degradable: &[REDIS], // presence降级不影响消息发送
    })
    .handler(handler_fn);
```

**选项 A**：显式的 .with_dependencies() 声明（如上）  
- 优点：自文档化，可在 CI 中做依赖覆盖检查  
- 缺点：需要修改 Router 构建链，侵入较大  

**选项 B**：反射式，从 handler 的代码路径中推导依赖（实际调用到 Redis 即视为依赖于 Redis）  
- 优点：零改动  
- 缺点：不精确，无法区分「需要」和「可以绕过」  

**选项 C**：指标驱动，不声明依赖，只在调用点检测故障并按故障类型降级  
- 优点：最小改动  
- 缺点：降级行为不可预测  

**推荐选项 A** 的轻量版本——不需要改 `Router` 构建方式，改在 `HealthRegistry` 上注册：

```rust
health_registry.register_dependency(DependencyKind::Redis, 
    DependencyPolicy::Degradable {
        degraded_mode: DegradedMode::PresenceStale,
        fallback_fn: || hub.local_presence(), // 降级时的替代策略
    }
);
```

### 3.2 凭据存储接口

**原则**：`CredentialStore` 是 trait，不是 struct。当前可以有且只有一个实现（EnvStore），但接口定义要从一开始支持切换。

```rust
#[async_trait]
trait CredentialStore: Send + Sync {
    /// 获取凭据值
    async fn get(&self, key: &CredentialKey) -> Result<Arc<str>, CredentialError>;
    
    /// 可选的吊销方法
    async fn revoke(&self, key: &CredentialKey) -> Result<()> { Ok(()) }
}

enum CredentialKey {
    TurnSecret,
    AiAnthropicKey,
    AiOpenAiKey,
    JwtSecret,
    InternalBridgeSecret,
    // ...
}
```

**关键设计决策**：返回值是 `Arc<str>` 而非 `String`。——凭据是频繁读取的（每次 `/api/rtc-config` 请求读取 TURN secret），用 `Arc` 减少克隆。但隐含着凭据轮换后，已有 `Arc` 引用仍保持旧值，直到所有引用被 drop 后才释放。这对于安全审计是可接受的——轮换的凭据不溯及既往，新连接使用新凭据。

### 3.3 广播限流接口

**原则**：不与通用 rate limiter 耦合（当前 `AERO_RATE_LIMIT_PER_SEC` 是每个请求/连接的限流，不区分操作语义）。

```rust
struct BroadcastGuard {
    room_id: RoomId,
    sender_id: ParticipantId,
    member_count: u64,
}

impl BroadcastGuard {
    /// 检查是否允许发送广播
    async fn check(&self, state: &AppState) -> BroadcastDecision;
}

enum BroadcastDecision {
    Allowed,                         // 直接通过
    DowngradeToOnline,               // @everyone → @here
    Rejected { reason: &'static str, retry_after: Option<Duration> },
}
```

## 四、技术选型

### 4.1 是否需要引入新技术栈

**不需要**。五个方向均可在现有技术栈内解决：

| 方向 | 新增依赖 | 理由 |
|------|---------|------|
| 断路器/降级 | 零新增 | `tokio::sync::watch` + tokio timer 即可实现。不需要 `resilience4j` 类框架 |
| 凭据管理 | 零新增 | 本地 `watch` 文件 + TTL 缓存即可。Vault 集成是可选下游 |
| 广播限流 | 零新增 | 现用 Redis 滑动窗口（已有 rate limiter 模式，复用即可） |
| 客户端收敛 | 零新增 | 纯 JS 变更 |
| 协议 handler | 零新增 | 纯 JS 变更 |

当前的 Rust 技术栈（tokio + axum + sqlx + fred + async-nats）完全覆盖所需。核心投入是**设计模式**而非**依赖采购**。

### 4.2 评估新技术引入的标准

如果未来考虑引入新技术，应遵循以下标准：

| 标准 | 权重 | 说明 |
|------|------|------|
| **zero-trust 依赖** | 高 | 优先选纯 Rust 实现（已有 str0m、rmp_serde 先例）。避免 C 绑定（尤其涉及安全降级逻辑时）。 |
| **运行时开销可预期** | 高 | 断路器/降级逻辑必须是无锁或低竞争的——不可以在每个请求路径上加全局锁。 |
| **部署模型匹配** | 中 | 当前是单二进制 + 水平扩展，新增技术应可嵌入进程（如 Vault agent sidecar），不引入独立服务依赖。 |
| **学习曲线** | 低 | 不引入新概念（如 Polyglot 特性、DSL 配置语言）。新增模式应是现有开发者熟悉的。 |

### 4.3 自建 vs 采购决策

| 能力 | 建议 | 理由 |
|------|------|------|
| 断路器 | **自建** | 需要的是轻量级模式（连续失败→休眠→探活），不需要 Hystrix 级别的完整实现。100 行状态机 + `tokio::sync::watch` 即可。 |
| 凭据存储 | **自建（先本地，后对接）** | 初始实现是 `AERO__` 环境变量的 TTL 缓存 read，不增加任何外部依赖。未来可对接 Vault/KMS 而不改变接口。 |
| TURN 服务器 | **采购/现有** | 不是自建项目。现有 Coturn 或 LiveKit TURN 即可。项目只需要实现客户端凭据签名。 |
| 分布式限流 | **用 Redis** | 已有 Redis + fred，滑动窗口计数器是标准模式。 |

**关键自建决策**：断路器模式值得自建的原因在于——它的正确性取决于应用语义（什么算失败？降级多久？探活成功后退出的阈值？），这些语义参数因业务而异，第三方框架难以通用化。

---

## 五、实施路线图

### 5.1 优先级重排

我基于两个维度重新排序：**规避风险的紧迫性** 和 **投入产出比**。

| 优先级 | 方向 | 估计投入 | 决策逻辑 |
|--------|------|---------|---------|
| **P0** | 方向一：TURN HMAC 凭据 | 1-2 天 | 安全债务，可逆窗口正在关闭。在生产环境中部署 WebRTC 前必须解决。 |
| **P0** | 方向四+三：客户端收敛（MessageSeen + delivery_cursor + Interaction） | 3-5 天 | 投入产出比最高——基础设施已就位，纯前端变更即可消除两处明显的 UX 断裂。快速回本。 |
| **P1** | 方向五：断路器 + 健康注册器（关键路径保护） | 1-2 周 | 需要在 Redis / NATS / AI 三条关键路径上加保护，但需要先完成依赖分析，不宜跳过。 |
| **P1** | 方向二：@everyone 成本控制 | 3-5 天 | 在群规模增长前可以缓一缓，但不能超过本季度。 |
| **P2** | 方向五扩展：完整降级策略（所有 bot/worker/定时器） | 2 周 | 在核心保护就位后扩展到全系统。 |
| **P2** | 凭据管理层泛化（CredentialStore + Vault 集成） | 1-2 周 | 在 TURN 凭据改造完成后，积累经验再抽象。 |

### 5.2 阶段划分

#### 阶段一：低挂果实 + 安全止血（1 周）

**目标**：消除最高风险的架构缺口，快速释放积压的价值。

| 周 | 工作项 | 交付物 |
|----|--------|--------|
| Day 1-2 | TURN HMAC 签名 | `AERO_TURN_SECRET` env var，`/api/rtc-config` 返回 HMAC 签名的短时效凭据 |
| Day 1-2 | 客户端 MessageSeen + delivery_cursor 收敛 | `web/app.js` 的 `ws.on('msg:message_seen')` handler + `hookWs` open 时拉取 cursor |
| Day 3 | 客户端 Interaction handler | `ws.on('msg:interaction')` 渲染实时反馈 |
| Day 4-5 | 客户端 Welcome handler + 多设备已读推送校准 | Welcome 帧校验 + `_lastSeen` localStorage 持久化 + markRead 时的跨设备 Read 帧消费 |

**风险**：客户端 handler 多但零服务端变更，可并行开发。主要风险是 JS 端的边缘 case（如 WS 重连后 cursor 同步时序）。**缓解**: 在「已读」功能上做分级发布——先对 10% 用户开放，观察无异常后全量。

#### 阶段二：韧性骨架（2 周）

**目标**：建立系统的降级框架，保护关键路径。

| 周 | 工作项 | 交付物 |
|----|--------|--------|
| Week 1 | 健康注册器 + 断路器状态机 | `HealthRegistry`（`Arc<RwLock<HashMap<DependencyKind, CircuitState>>>`） |
| Week 1 | PG/Redis/NATS/AI 探活集成 | 每个依赖的健康检查函数注册，定时探测 |
| Week 2 | 关键路径降级实现 | presence 降级（Redis 故障→本地 Hub）、NATS publish 降级（待广播队列）、AI 断路器 |
| Week 2 | `/health/degraded` + WS `degraded` 帧 | 运维端点 + 客户端黄色横幅 |

**风险**：降级逻辑的正交性——如果一个依赖被标记为降级，多个 handler 都会读这个标记并各自触发降级。如果某个 handler 降级后有 bug，影响面可能被放大。**缓解**：先对只读路径做降级（presence 查询、stream viewer count），写路径降级（NATS 本地缓冲）推后到阶段三。

#### 阶段三：防御深化 + 泛化（2 周）

| 周 | 工作项 | 交付物 |
|----|--------|--------|
| Week 1 | @everyone 广播限流 | Redis 滑动窗口 + 成员数门控 + 降级为 @here |
| Week 1 | notification 批量写入 | `INSERT INTO notifications (...) SELECT ... FROM unnest($1)` 替代逐条 insert |
| Week 2 | 完整降级扩散 | 所有 bot/worker 加入断路器保护（agent_bot、moderation_bot、push_bot 等） |
| Week 2 | 凭据管理层抽象 | `CredentialStore` trait + `EnvCredentialStore` 实现 + 凭据热重载 |

**风险**：批量化 notification 写入需要修改 `notification_repo` 的接口，涉及迁移测试数据。**缓解**：纯 SQL 变更，不改变业务逻辑，在 staging 全量测试后上线。

### 5.3 风险矩阵

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|---------|
| TURN HMAC 实现 + Coturn 配置不兼容 | 中 | 高 | 先 mock TURN server 做集成测试；配置文档预写 GitBook 格式 |
| 断路器降级后恢复逻辑导致抖动 | 中 | 中 | 使用 `half-open + min_success_count` 模式，不直接 full-open→closed |
| delivery_cursor 写入在 WS 重连时序下丢失 | 低 | 中 | `_lastSeen` 写入 `localStorage` + 服务端 `delivery_cursors` 双写入，以较大者为准 |
| @everyone 限流在大群场景误拦截正常广播 | 中 | 中 | 阈值可配置，默认宽松（100 成员 + 1/60s）；fail-open（限流不可用时放行） |

### 5.4 成功指标

| 方向 | 可验证指标 | 目标值 |
|------|----------|--------|
| TURN HMAC | `/api/rtc-config` 返回的 credential 每会话不同 | 100% 不同 |
| TURN HMAC | credential 24h 后返回 401 | 自动化测试 |
| 断路器 | Redis 宕机 3s 内 presence 降级，无崩溃 | 响应码保持 200（stale 数据） |
| @everyone 限流 | 同一房间在 60s 内第二次 attempt 返回 429 而非写库 | 集成测试 |
| 客户端收敛 | 设备 A markRead → 设备 B 刷新后未读计数降为 0 | E2E 测试 |
| message_seen | 消息底部显示 ≥1 人已读 | 视觉测试 |

---

## 总结

这五个方向有一个共同特征：**它们不是新功能，而是既有能力的完整性闭合**。TURN 是安全闭合、断路器是韧性闭合、`@everyone` 限流是运维闭合、客户端 handler 和 cursor 是协议闭合。项目代码库的工程质量相当高——这些缺口不是因为做得不好，而是因为架构演进的自然节奏总是先构造主干、再生长支脉。现在是这些支脉结果的时候了。

最值得注意的洞察：**方向四和方向三的修复可以在同一次发布中完成**，因为它们都是客户端一端的工作——服务端端的代码（帧定义、总线广播、delivery_cursor 表）已有多年服役历史，只是缺乏消费者的「最后一公里」。这两项修复的投入产出比（3-5 天换取两个产品级的 UX 提升 + 协议完整性的消除）是本期所有方向中最高的。

韧性策略的方向五则应被视为**持续投资**而非单次项目。断路器不是「装完就不管」的——它需要持续的调优（失败阈值、休眠时长、探活间隔）和运维告警集成。建议将其作为架构能力而非功能特性来投入资源。
