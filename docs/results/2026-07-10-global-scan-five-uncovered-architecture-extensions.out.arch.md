现在我已充分理解系统的架构和文档中指出的缺口。以下是我的分析。

---

# 架构师分析：Aero IM 架构扩展方向评估

## 1. 架构评估

### 1.1 当前架构的优势

从代码证据和文档来看，该系统的架构基础相当扎实：

| 优势 | 代码级证据 | 战略价值 |
|------|-----------|---------|
| **严格的分层** | 16 crate 按「基础层 (common/bus/storage/auth) → 业务层 (im-core/im-call/ai) → 组合层 (server)」排列，无成环 | 依赖关系可独立演进、替换、测试 |
| **事件驱动的事实源** | NATS JetStream + 两阶段解码 (`run_bus_listener`)，at-least-once 耐久消费者 | 跨实例一致性、可回放、可审计 |
| **进程内扇出（Hub）不依赖外部中间件** | `Hub::fan_out_raw` bounded mpsc 本地扇出 | 延迟可控，不将 WS 连接生命周期绑定到 NATS 可连接性 |
| **特征门控与自省清晰** | Redis sorted-set 作为集群状态、AGENTS.md 明确区分进程内 vs 集群级状态 | 运维人员可预测行为 |
| **bot/agent 系统的隔离设计** | 8 个独立总线消费者，各有独立的 durable consumer name 和 fail-open 策略 | 可独立演进、独立 fail、逐个替换 |
| **缓存与回退策略（AI 侧）** | `HashEmbedder`（无 key→确定回退）和 `AiWorker` 预算限制 | 系统的 AI 模块已有清晰的降级契约——唯一一个有显式降级层级的部分 |

### 1.2 当前架构的局限性

文档已识别 5 个缺口，我将其归为两层：

**第一层：产品成熟的断层**（直接影响用户留存和采用）

| 缺口 | 类型 | 根本原因 |
|------|------|---------|
| SPA 无路由/深度链接 | 前端架构 | 早期原型阶段的 `hidden` 视图切换未被重构——功能叠加时没有回头修复路由 |
| API 版本治理缺失 | API 基础设施 | 126 个模块全部裸 `/api/*` 路径——没有区分「内部 API」和「公共 API」的意识 |

**第二层：运维成熟的断层**（影响可靠性和团队效率）

| 缺口 | 类型 | 根本原因 |
|------|------|---------|
| 多实例缓存一致性 | 分布式系统 | 设计时假设单实例部署——三个缓存都写成进程内 DashMap，失效未跨进程 |
| 性能基准体系 | 工程实践 | 功能覆盖优先于性能可观测——817 个 UT 但零 bench |
| 降级/熔断决策 | 可靠性工程 | 无显式降级矩阵——仅 AI 模块有降级路径 |

### 1.3 关键设计决策的合理性评估

| 决策 | 当时合理 | 当前评估 | 建议 |
|------|---------|---------|------|
| **无 SPA 路由**（纯 JS 状态 hidden 切换） | ✅ 早期原型，快速迭代 | ❌ 对用户留存有实质伤害 | P1 修复 |
| **裸 `/api/*` 路径** | ✅ 单客户端、同部署 | ❌ 阻碍多客户端的自然演化 | P2 修复，但不需一次性全迁 |
| **进程内 TTL 缓存** | ✅ 减少 PG/Redis 查询 | ❌ 多实例下产生可见不一致 | P2 修复，低风险高回报 |
| **无性能基准** | ✅ 功能验证优先 | ❌ 无法评价变更对性能的影响 | P2 修复，逐步建立 |
| **AI 模块有降级路径，其余无** | ✅ AI 是差异化功能 | ❌ 其他依赖无降级策略 | P2 修复，重心在决策矩阵而非断路器库 |

---

## 2. 扩展方向

基于上述评估，我从文档的 5 个方向中提取以下 3-5 个高价值扩展方向。

### 方向一（P1 · 产品层）：SPA 路由与深度链接

**为什么需要**

- **业务价值**：深度链接对任何 IM 应用都是基线 UX。没有它，推送通知、邮件通知、消息分享、浏览器书签的转化率大幅降低。文档引用的"40-60% 通知点击下降"估计合理——这是数据驱动的决策。
- **技术价值**：`hashchange` + `history.pushState` 的引入是前端架构的**规范化窗口**——一旦引入哈希路由，message-link 协议、thread 路由、stream 路由都会在同一个框架下自然生长。

**核心挑战**

1. **WebSocket 生命周期管理**：跨房间导航不应重新建立 WS 连接——需要 `join_room` 消息而非重新 `upgrade`。当前 `app.js` 的 `connectWebSocket()` 函数在页面加载时建立连接，需要在路由变化时复用。
2. **认证后路由恢复**：深度链接若指向私密房间，未登录用户必须先认证再导航到目标。需要存储 `pendingRoute` 并在 `showAuth()` → `showChat()` 转换时消费它。涉及 `state` 对象的扩展。
3. **推送通知点击与深度链接的集成**：`notifications.js` 中的 `handleNotificationClick` 需要从 `room_id` 数据构造 URL 并调用路由导航——当前是空窗口。

**预期架构变更**

- **新增**：`web/router.js`（~100 行），实现路由表 + `hashchange` 监听 + `navigate()` 函数。
- **修改**：`web/app.js`——将 `showChat()` 和 `showAuth()` 改为由路由驱动而非直接调用；修改 ws 连接初始化逻辑为惰性复用。
- **新增**：`web/deeplink.js`（~50 行）——处理 `#/room/:id/message/:mid` 等链接的解析和导航。

**对现有系统的影响**

| 影响面 | 评估 |
|--------|------|
| 后端 | **零**——纯前端改动 |
| 现有 JS 代码 | `state` 对象只新增 `router` 字段，不删除/重构任何现有逻辑 |
| WebSocket 连接 | 仅修改建立时机——从「页面加载时」改为「页面加载 + 路由导航前」。对现有消息收发零影响 |
| 测试 | `web/` 目录无测试（见文档 `cargo test --workspace` 仅 Rust），无需考虑测试兼容性 |
| 风险 | 低——纯新增 + 功能兼容性（旧行为全部保留） |

### 方向二（P2 · API 基础设施）：API 版本治理与兼容性契约

**为什么需要**

- **企业合同门槛**：文档指出「多家企业客户合同已含 API 稳定性条款」。在许多企业采购流程中，无版本化的 API 直接使安全/架构审查不通过——这是合同签署的前置条件，不是可选项。
- **多客户端协调**：Web SPA 可随部署一起更新，但移动端（规划中）有审核周期，第三方 Bot SDK（已有 `agent_bot.rs`）有升级窗口。没有版本切换意味着每次后端变更都是迫在眉睫的 breakage。

**核心挑战**

1. **92 个路由的手工版本化不现实**——126 个 `.merge()` 模块，手动标注每个 handler 的版本是不可扩展的。需要**分组级的版本策略**：大部分路由版本迁移由中间件/RouteLayer 自动处理，只有数据结构变更（如 `RoomEvent` 的 `kind` tag 变化）才需要 handler 级别的适配。
2. **非对称版本策略**：公开 API（消息、协作、用户资料）需要版本化 + deprecation 协议；内部 API（清扫定时器、管理 endpoints、系统间通信）保持无版本。但两者在 routes.rs 中纠缠在一起——126 个模块在同一条 `.merge()` 链上，无法简单加前缀区分。
3. **版本共存时的资源占用**：每个活跃版本都是一个独立的路由树。如果 `v1` 和 `v2` 共享相同业务逻辑（`ImService`），但序列化契约不同，需要序列化层适配器。需要避免复制 handler 代码——**版本只影响 I/O 边界，不影响核心逻辑**。

**预期架构变更**

- **新增**：`crates/aero-server/src/routes/versioning.rs`——版本解析中间件（从 header `Accept-Version` 或 `/api/v2/` 前缀提取版本号）→ 注入 `req.extensions()`。
- **新增**：`crates/aero-server/src/api_compat/` 目录——版本适配层（`v1.rs` + `v2.rs`），包装 `ImService` 的返回值以兼容旧序列化格式。
- **修改**：`routes.rs`——在 `build()` 中为 `v1` 和 `v2` 各生成一个 `Router`，然后合并。非公开路由（管理/admin/清扫）挂在 `/api/internal/` 下，无版本前缀。
- **新增**：`AeroDeprecation` middleware——检查路由的 `Sunset` 信息，在响应头中添加 `Deprecation: true` 和 `Sunset: ...`。

**对现有系统的影响**

| 影响面 | 评估 |
|--------|------|
| 现有路由 | 所有 126 个模块初期在 `v1` 下相同注册——零功能回归 |
| handler 代码 | **零改动**——版本化只影响路由注册位置，不修改 handler 签名 |
| 现有客户端 | 继续使用 `/api/...`（内部重定向到 `/api/v1/...`），向后兼容 |
| 测试 | 新增 `tests/api_versioning.rs` 验证版本头路由和 deprecation 头 |
| 风险 | 中低——新增中间件 + 新增路由注册层，不触及业务逻辑 |

**选项与权衡**

| 选项 | 优点 | 缺点 |
|------|------|------|
| **Header-driven**（`Accept-Version: v2`） | 干净的 URL 设计；版本与资源路径解耦；符合 REST 最佳实践 | 浏览器以外的客户端需要显式设置 header；Swagger/OpenAPI 工具链对 header 路由支持较弱 |
| **URL 前缀**（`/api/v2/rooms/...`） | 工具链友好（curl/浏览器直接测试）；URL 本身可缓存/CDN-friendly | URL 设计污染（每个路由多一层嵌套）；版本爆炸（`/api/v2/v1/...` 的风险） |
| **混合**（推荐） | 默认版本（无 version header → `v2`）；`Sunset` header 告知旧版本过期 | 需要维护版本注册表；需区分默认版本与最新版本 |

### 方向三（P2 · 架构/稳定性）：跨实例缓存失效（Cache Coherence）

**为什么需要**

- **多实例是企业部署的默认场景**——NATS 集群 + 多 `aero-server` 实例的设计（`run_bus_listener` 使用 durable consumer `aero-server`）表明系统从一开始就被设计为水平多实例。两个进程内缓存的失效只清本地进程，与这一前提矛盾。
- **实际用户可见 bug**：`participant_cache` TTL = 60s。实例 A 更新头像后，实例 B 最多 60s 内显示旧头像。此 bug 属于「极难复现、定位、解释」的那一类——但代码已证明其必然发生。

**核心挑战**

1. **失效风暴控制**：批量操作（如工作区批量更新 100 个成员角色）会触发 100 次 `invalidate` → 100 次 Redis pub。需要去重（`debounce` 或 `batch` 接口）。
2. **失效信道的高可用**：Redis pub/sub 是 at-most-once（Redis 集群模式下甚至有消息丢失风险）。如果失效丢失，系统退回到 TTL 的最终一致性——可接受但不理想。可以考虑双重信道（Redis pub + NATS JetStream）但会增加复杂度。
3. **本地缓存的粒度与频率**：当前 `participant_cache.get_or_fetch` 按 `ParticipantId` 单条加载。如果每条消息都触发多人查询，失效广播可能成为新的热路径。

**预期架构变更**

- **新增**：`crates/aero-server/src/cache_invalidation.rs`——`CacheInvalidationBus` trait + 实现 `RedisPubSubInvalidator`。
- **修改**：`participant_cache.rs`——`invalidate()` 改为调用 `cache_invalidation_bus.invalidate(pid)` + 本地 `inner.remove(&pid)`。
- **修改**：`room_member_cache.rs`——同上。
- **新增**：`boot/background.rs` 或 `persistence.rs`——启动时订阅 Redis channel `cache:inval:*`，收到消息后执行本地 `invalidate`。
- **可选**：`DashMap` 的 slim-watch 模式——监听每个 key 的变更而不扫全表。

**对现有系统的影响**

| 影响面 | 评估 |
|--------|------|
| 现有 API 路由 | 写 participant 的路由已经调用 `invalidate()`——只需在旁增加广播调用 |
| Redis 连接 | 已有 `fred` 客户端（用于 `PresenceStore`），复用同一连接池——零新连接 |
| 性能 | Redis pub 是 O(1) 操作 - 每次写入增加亚毫秒延迟 |
| 单实例部署 | 不影响——无其他实例消费失效广播，退回到 TTL 最终一致性（与当前行为相同） |
| 风险 | 低——新增独立模块、不改现有缓存行为、失效丢失时安全降级到 TTL |

### 方向四（P2 · 工程实践）：性能基准体系

**为什么需要**

- **防止静默性能退化**：文档指出「一个看似无害的 PR（加一个 SQL JOIN、加一个 Redis 查询）可能引入 50% 的 P99 延迟回归而 CI 无法捕获」。这是所有生产系统的真实痛点——单元测试不捕获性能退化。
- **容量规划的输入**：没有基准数据，无法回答「一台机器支持多少连接」。当前系统已讨论多区域和 99.9% SLA——没有吞吐数据，这些讨论是无根基的。

**核心挑战**

1. **微基准的可靠性**：Rust 的 criterion/divan 微基准要求受控环境——CPU 频率缩放、ASLR、其他进程干扰。CI 容器高度不可控，`criterion` 的相对比较（baseline vs PR）在 CI 中仍然可用，但绝对数值不可靠。
2. **端到端基准的环境一致性**：`k6` 负载测试需要固定的数据库规模（如 10 万条消息、1000 个房间）来保证可重复性。每次 CI 运行前需要重建测试数据。
3. **性能预算的维护成本**：每个关键端点定义 P99 预算容易（< 100ms），但预算过紧导致 CI 不稳定、过松无意义。需要定期校准。

**预期架构变更**

- **Cargo.toml**：新增 `[dev-dependencies]` 下的 `criterion`（`crates/` 级别的选择性依赖，不污染根 workspace）。
- **新增**：`crates/aero-bus/benches/seq.rs`（序列化/seq 微基准）
- **新增**：`benches/message_ingest.rs`（消息全链路基准）
- **CI 改动**：`.github/workflows/ci.yml` 新增 `bench` job——`cargo bench --workspace`，结果与基线比较，回归 > 5% 标记警告。
- **新增**：`scripts/perf-budget.yaml`——声明每个关键端点的 P99 延迟预算。

**对现有系统的影响**

| 影响面 | 评估 |
|--------|------|
| 现有代码 | 零改动——纯新增 bench target 和 CI job |
| 编译时间 | 微基准对编译时间影响可忽略（dev-deps only）；端到端基准（k6）是独立进程 |
| CI 时间 | 新增 job 增加 3-5 分钟（`cargo bench` 编译 + 运行）。建议 nightly-only 或 weekly 基准，CI 仅检查 bench 编译通过 |
| 风险 | 极低——纯新增、独立于主代码路径 |

**建议的基准优先级**（按价值递减）：

| 优先级 | 基准 | 工具 | 价值 |
|--------|------|------|------|
| 1 | 消息序列化反序列化（`RoomEvent` + `StreamEvent`） | criterion | 捕捉 serde tag 变更或复杂嵌套引入的延迟回归 |
| 2 | 消息全链路发送（WS → PG INSERT → NATS → fan-out） | k6 | 核心数据平面 P99 预算 |
| 3 | 搜索查询（FTS + vector + hybrid） | k6 | 最复杂的 SQL 查询路径 |
| 4 | AI 嵌入（`HashEmbedder` 基准） | criterion | AI 降级路径的延迟基线 |
| 5 | WebSocket 连接数/吞吐 | k6 | 容量规划的基线 |

### 方向五（P2 · 可靠性）：依赖降级分层与自适应熔断

**为什么需要**

- **99.9% SLA 需要「部分可用 ≠ 不可用」**——每个依赖故障时全部 500 返回，意味着 SLA 计算是脆弱的。Redis 抖动 30 秒不应该触达用户可见的故障。
- **唯一有降级路径的是 AI 模块**——`HashEmbedder` + 启发式 completion 已在文档中标为"沙箱 AI 路由也应 200"（AGENTS.md §4.2）。但其他依赖（Redis、NATS、PG）没有同等的降级策略。

**核心挑战**

1. **降级决策矩阵的「全局可见」**：降级状态（L1/L2/L3）需要在 `AppState` 中共享，所有中间件和 handler 可见。当前 `AppState` 没有这种枚举。
2. **熔断参数难以通用**：三个依赖（PG、Redis、NATS）的不同故障模式需要不同的熔断参数——连接超时 vs 查询超时 vs 连接拒绝，连续失败 N 次 × 窗口 M 秒。
3. **降级后的恢复策略**：熔断进入半开后，探测请求的成功率阈值如何决定？Redis 和 PG 更倾向于快速恢复（窗口短、阈值低），AI 供应商 API 更倾向于保守恢复（窗口长、阈值高）。
4. **降级状态的用户面**：降级后，用户 UI 需要看到「AI 摘要暂不可用，已切换到本地摘要」的提示。当前 `web/app.js` 无任何降级提示 UI（仅有 WS 连接状态的点指示器）。

**预期架构变更**

- **新增**：`crates/aero-common/src/degradation.rs`——`DegradationLevel` 枚举（`Normal | Limited(&'static str) | ReadOnly | Offline`），全局可见。
- **新增**：`crates/aero-server/src/circuit_breaker.rs`——轻量断路器 state machine，无第三方依赖（手写 ~150 行 vs 引入 `failsafe` crate）。
- **修改**：`AppState`——新增 `degradation: Arc<RwLock<DegradationLevel>>`。
- **新增**：`web/banner.js`——降级提示 UI（顶部 banner + toast），接收来自 WS 的 `server_degradation` 帧或 REST `/health?fields=degradation`。
- **修改**：`routes/health.rs`——`/health` 响应包含 `degradation_level` 字段。

**对现有系统的影响**

| 影响面 | 评估 |
|--------|------|
| 现有 handler | 仅 L3（灾难模式）需要修改 handler——L1/L2 变化对 handler 透明，由中间件层处理 |
| 现有依赖调用 | Redis/NATS/PG 调用需要包裹 `CircuitBreaker::call_or_fallback()`——初步覆盖核心读路径即可 |
| 现有配置 | 新增 `[degradation]` 配置节（熔断阈值、窗口大小） |
| WS 协议 | 新增 `ServerFrame::DegradationUpdate` variant——需要 `ws/ws_impl/bus.rs` 扩展 |
| 风险 | 中——涉及多个层的修改（common、server、ws、web），但可按「先声明矩阵→再实现断路器→最后 UI」分步 |

**降级矩阵草案**

| 依赖 | 正常（L1） | 受限（L2） | 灾难（L3） |
|------|-----------|-----------|-----------|
| **PG** | 全部功能 | 非关键查询降级（搜索 → 基本文本搜索、AI 异步队列暂停）、清扫定时器暂停 | **只读模式**——消息可读不可写，历史查询可用但新消息无法发送 |
| **Redis** | 全部功能 | Rate limiter 回退到 DashMap（当前隐式行为→显式决策）Presence 回退到 PG 查询 | Presence 全部离线、rate limiter 退守到保守上限（全局硬速限） |
| **NATS** | 全部功能 | Bot/agent 暂停、直播弹幕暂停（本质无法扇出）；但 WebSocket 直通模式启用（直接 Hub 扇出，仅本实例可见） | 相同（NATS 不可用时跨实例通信不可能） |
| **AI API** | Anthropic + Voyage | 退化到 `HashEmbedder` + 启发式 completion（已有实现） | AI 功能全部禁用 + UI 提示 |

---

## 3. 接口设计建议

### 3.1 现有关键模块的接口原则

当前系统在 crate 边界上使用 Trait + 结构体模式（如 `EventBus` trait、`BlobStore` trait），这是正确的方向。但有以下接口值得重新审视：

**建议一：为 `CacheInvalidationBus` 定义 trait，而非直接耦合 Redis pub**

```rust
// 建议的 trait 定义（非代码，接口级设计）
trait CacheInvalidationBus: Send + Sync + 'static {
    fn invalidate_participant(&self, pid: ParticipantId);
    fn invalidate_room(&self, rid: RoomId);
    fn subscribe(&self) -> BoxStream<'_, InvalidationEvent>;
}
```

- 两种实现：`RedisPubSubInvalidator`（多实例）、`NoopInvalidator`（单实例/测试）
- 保持 `participant_cache::invalidate()` 签名不变——`CacheInvalidationBus` 在 boot 时注入

**建议二：降级/熔断按模块切分，而非单一全局熔断器**

- 每个外部依赖（PG、Redis、NATS、AI）有自己的 `CircuitBreaker` 实例
- 全局 `DegradationLevel` 是这些熔断器的**聚合状态**（如果有 ≥1 个熔断器打开且影响核心路径，则升级到 L2/L3）

**建议三：API 版本化的侵入点最小化**

- 不修改 handler 签名
- 版本选择在 `RouteLayer` 中间件中完成（检查 header/URL 前缀 → 注入 `ApiVersion` extension）
- 版本适配层（`v1.rs` / `v2.rs`）在 handler 的 `IntoResponse` 层操作——handler 返回内部数据结构，version adapter 序列化为不同格式

### 3.2 是否需要新的抽象层

| 建议新增的抽象层 | 理由 | 复杂度评估 |
|-----------------|------|-----------|
| **`CacheInvalidationBus` trait** | 当前缓存失效直接耦合到 DashMap 的 `remove()`——需要一个抽象来支持多实例广播 | ~50 行 trait + 2 个实现 |
| **`CircuitBreaker` struct** | 当前无断路器——每个外部依赖调用直接透传错误。手写 state machine（Closed/Open/HalfOpen）比引入 `failsafe` crate 更轻量 | ~150 行 |
| **`RouteVersionLayer` middleware** | 当前无版本路由——需要在中间件层拦截版本选择，而不是在每个 handler 中手动判断 | ~80 行 Axum middleware |
| **`SpaRouter` 前端模块** | 当前无路由——纯原生 `hashchange` 实现 | ~100 行 JS |

**建议不新增的抽象**：

- **不引入 `ServiceMesh` 层**（Istio/Linkerd）——当前系统需要的是应用层的断路器决策，而非网络层的流量管理。Service Mesh 无法判断业务层降级逻辑（如 AI 后端超时应该降级到 `HashEmbedder`）。
- **不引入 `APIGateway` 层**（Kong/Tyk）——当前的 Axum 网关已经是耦合到业务的路由层，再引入外部 API 网关会增加运维复杂度而非降低。版本化在应用层中间件完成更轻量。

### 3.3 如何保持向后兼容性

| 变更 | 兼容策略 | 过渡期 |
|------|---------|--------|
| 新增哈希路由 | 旧 `hidden` 视图切换保留，新路由绑定到 `hashchange` 事件——旧行为未被删除 | 无限（两模式共存，互不影响） |
| API 版本化 | 初始全部挂 `v1`；空版本请求通过重定向中间件转到 `v1` | 6-12 个月的 deprecation 窗口 |
| 缓存失效广播 | 单实例部署无其他消费者——退回到 TTL 最终一致性（与当前相同） | 永久兼容 |
| 断路器/降级 | L1（正常）路径不变；L2/L3 只在断路器打开时才激活——正常时零影响 | 断路器关闭时零开销 |

---

## 4. 技术选型

### 4.1 是否需要引入新的技术栈

| 方向 | 建议 | 理由 |
|------|------|------|
| SPA 路由 | **自建（纯原生 JS）**——零额外依赖 | `hashchange` 事件 + `pushState` 是浏览器原生 API。引入 React Router / Vue Router 需要整套框架迁移（5000 行 JS 到框架的天花板极高），ROI 为负 |
| API 版本化 | **自建中间件**——无需框架 | Axum 的 `middleware` + `RouteLayer` 完全够用。引入 `utoipa`/`okapi`（OpenAPI 生成）是合理的补充，但要配合手写路线 |
| 缓存失效 | **自建 trait**——轻量 | Redis pub/sub 已是可用信道。引入 Hazelcast/RedisGears/Redis Streams 会大幅增加架构复杂度 |
| 性能基准 | **criterion**（Rust 微基准）+ **k6**（端到端负载测试） | criterion 是 Rust 生态事实标准（无 unsafe，zero-cost），k6 是 JS 脚本化的负载测试工具，与现有 smoke 脚本工具链一致 |
| 断路器/降级 | **自建（有限状态机）**——约 150 行 | `failsafe` crate（Rust 生态中最常用的断路器库）是 < 100 行的 state machine wrapper。引入它 vs 自建的差别不大。自建的优势是可以直接集成到降级矩阵决策 |

### 4.2 第三方依赖的评估标准

对于任何新引入的依赖，建议按以下标准评估：

| 标准 | 权重 | 说明 |
|------|------|------|
| **Rust 版本兼容性**（MSRV 1.80） | 硬性 | 依赖必须兼容 workspace `rust-version` |
| **unsafe 代码量** | 高 | 当前 workspace `unsafe_code = "forbid"`——首选 zero-unsafe 依赖 |
| **维护活跃度** | 中 | 最后更新 > 2 年的依赖视为维护风险 |
| **依赖树膨胀** | 中 | 避免引入大框架（如 Tokio Console、Tonic）来换取小功能 |
| **license 兼容性** | 低 | MIT/Apache 2.0 首选（与 workspace 一致） |

### 4.3 自建 vs 采购的决策框架

当前五个方向都是「自建」场景——它们是架构片段（路由逻辑、缓存失效协议、断路器 state machine），而非独立可采购的产品。未来当系统扩展到**联邦/多区域/数据主权**等方向时，以下决策框架适用：

```
问题：需要解决什么不变量？
├── 数据平面不变量（消息持久性、一致性、顺序）→ 自建（核心差异化）
├── 控制面不变量（路由、认证、限流）→ 自建（不可委托业务逻辑）
├── 观测面（指标、追踪、告警）→ 采购/集成现有（Prom/Grafana/Jaeger → 已用）
└── 基础设施面（服务发现、配置管理、密钥管理）→ 采购/集成（K8s/Hashicorp Vault）
```

---

## 5. 实施路线图

### 5.1 优先级排序与阶段划分

```
P0（阻塞性）→ 直接影响用户体验或企业采用
P1（高价值） → 运维成熟度瓶颈
P2（重要）   → 非阻塞性，可并行推进
```

| 阶段 | 方向 | 工作量估计 | 依赖关系 |
|------|------|-----------|---------|
| **Phase 0（1-2 天）** | 方向一：SPA 路由基础（`#/room/:id` 哈希路由 + 历史导航） | 1 人 × 2 天 | 无——纯前端 |
| **Phase 1（3-5 天）** | 方向三：跨实例缓存失效 | 1 人 × 3-5 天 | 无——独立模块 |
| **Phase 2（5-10 天）** | 方向二：API 版本化声明 + 非对称分组 | 1 人 × 5-7 天 | 依赖 Phase 0 完成后前端对接 |
| **Phase 2（并行）** | 方向四：性能基准雏形（3 个微基准 + 1 个 k6 场景） | 1 人 × 3-5 天 | 无（与 Phase 2 其他并行） |
| **Phase 3（5-10 天）** | 方向五：降级矩阵声明 + 轻量断路器 | 1-2 人 × 5-10 天 | 依赖方向三（部分复用 `AppState` 扩展） |
| **Phase 4（持续）** | 方向一增强：深度链接（`#/room/:id/message/:mid` + 推送通知集成） | 1 人 × 3 天 | 依赖 Phase 0 |
| **Phase 4（持续）** | 方向二增强：deprecation header + 多版本共存（v1/v2） | 1 人 × 5 天 | 依赖 Phase 2 |
| **Phase 4（持续）** | 方向四增强：性能预算 CI 门禁 + 每周基准对比 | 1 人 × 3 天 | 依赖 Phase 2 |
| **Phase 4（持续）** | 方向五增强：用户面降级提示 UI + WS 帧 | 1 人 × 3 天 | 依赖 Phase 3 |

### 5.2 里程碑定义

| 里程碑 | 交付物 | 验证标准 |
|--------|--------|---------|
| **M0**（第 1 周） | 哈希路由 + 单房间深度链接 | 通过 URL `#/room/01HXXXX` 可直接导航到对应房间；浏览器后退/前进在房间间切换 |
| **M1**（第 2 周） | 跨实例缓存失效 | 两个进程同时运行后，实例 A 更新头像 → 实例 B 在 < 1s 内反映变更（当前 = 60s） |
| **M2**（第 3 周） | API 版本声明 + 非对称分组 | 126 个模块的 80% 隐藏到 `/api/v1/*`；新路由挂在 `v2`；无版本请求使用默认 `v2`；deprecation 头在 `v1` 上出现 |
| **M2**（并行） | 性能基准持续集成 | `cargo bench` 运行通过；关键路径有 +/- 5% 的回归检测；CI 中不失败（仅警告） |
| **M3**（第 4 周） | 降级矩阵 + 轻量断路器 | Redis 模拟故障后，rate limiter 和 presence 回退显式降级（而非隐式 DashMap 回退）；UI 显示降级提示 |
| **M4**（第 6 周） | 深度链接完整（含认证后恢复） | 未登录用户点击推送通知链接 → 登录 → 自动导航到目标房间 |

### 5.3 风险点与缓解策略

| 风险 | 等级 | 可能影响 | 缓解 |
|------|------|---------|------|
| **深度链接认证恢复引入安全漏洞**（未授权用户访问私密房间） | 高 | 认证绕过 | 路由解析时不做房间数据查询——认证后才导航；导航到房间时仍然走现有 `assert_room_access` 守卫（代码路径不变） |
| **版本化中间件降低所有请求的延迟**（每次请求都检查 header + 路由选择） | 中 | P99 增加 | 版本解析是 O(1) 查找（哈希表而非字符串匹配），对延迟影响 < 0.1ms。必要时可以缓存版本选择结果到 request extensions |
| **缓存失效广播在 Redis 抖动时丢失失效消息** | 低 | 临时不一致（退回到 TTL 最终一致性） | 可接受——当前无广播时也是 TTL 最终一致。建议增加失效消息去重（单调递增 seq）以避免重放 |
| **熔断器参数设置过紧导致频繁误熔断** | 中 | 功能可用性下降 | 初始参数应保守（连续 10 次失败 / 60s 窗口）；混入健康探测（`/health` 探针的响应码）作为熔断判断的辅助信号，而非仅依赖业务调用的错误率 |
| **多个方向并行引入导致团队上下文切换成本过高** | 中 | 交付延迟 | 建议只分配 1 个方向/人/周（而非每人并行 2-3 方向）。M0+M1 可在第 1 周由同一人完成（工作量小、技术栈不同但互不冲突） |

---

## 总结

这五个方向不存在「先修课程」上的强制依赖关系（除深度链接依赖基础路由外），可以**并行推进但串行交付**：

1. **立即行动（第 1 周）**：方向一（SPA 路由基础）+ 方向三（缓存失效）——低风险、高回报、独立模块。这两者都只需要少量代码变更，但用户可见性最高。
2. **短期（第 2-3 周）**：方向二（API 版本化）——影响面广但实现路径清晰（中间件层，不修改 handler）。方向四（性能基准）——可交由专门的人/角色独立完成。
3. **中期（第 4-6 周）**：方向五（降级/熔断）——最具架构深度，涉及多层的改动（common → server → ws → web），需要前面的基础设施（`AppState` 扩展、WS 帧扩展）支撑。

系统的架构骨架（事件驱动 + 分层 crate + NATS 事实源）是健康的。这五个方向不是「改骨」，而是**给骨架配上肌肉、神经和皮肤**——让系统从功能原型进化到生产就绪。
