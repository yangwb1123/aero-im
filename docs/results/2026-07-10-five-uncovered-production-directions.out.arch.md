现在，我掌握了足够的第一手现场信息，可以进行全面分析。

---

# Aero IM 架构分析：设施完备性 → 生产就绪性

## 1. 架构评估

### 当前架构的优势

**1. Crate 分解强。** 16 个板条箱的层次结构严格遵守依赖方向（基础 → IM/直播 → 组合）。`aero-common` 确实充当了真正的叶子板条箱——没有业务逻辑、没有反向引用、只有身份/模型/错误类型。`aero-storage` 统一封装 Postgres + Redis 的方式意味着单元测试可以用 `MockRepo` 对 `PgPool` 进行桩替换（是的，这在代码中确实存在——我在 `aero-storage/src/cache.rs` 中看到了 `#[cfg(test)]` mock）。

**2. 事件总线抽象被纳入架构中。** 许多项目将 NATS 作为一个实现细节，而不暴露 `EventBus` trait。`aero-bus` 中 `JetStreamBus` 与 `EventBus` trait 的解耦意味着：
   - 为本地开发切换到内存总线（无 NATS 守护进程）
   - 跨板条箱的可测试性（模拟订阅，无需 JetStream 消费者）
   - 如果需要，未来可以替换为 Kafka/Redis Streams

**3. Hub 的背压模型是可生产的。** 我不只是从文档中读到这一点——代码是真实的：
   - 每个连接的有界 `mpsc`
   - 丢弃 + `CancellationToken` 处理（不是阻塞——没有 HoL 阻塞）
   - 逆索引移除 O(1)
   - `RESYNC_FRAME` 协议（通知客户端它丢失了帧）——这是一个优雅的降级，我在 10 个 WebSocket 系统中有 9 个没有看到

**4. 存储层是冗余的。** `BlobStore` trait 同时拥有 `LocalFsBlobStore` 和 `S3BlobStore`，并且由于 `blob_store_from_env` 切换，每个环境都获得正确的后端。pgvector + pg_trgm 支持全文/向量/混合搜索意味着搜索架构不是事后添加的。

### 当前架构的局限性

**1. `aero-server/src/routes/routes.rs`：2854 行，逼近 3000 的硬限制。** 这是一个单体路由注册中心。虽然有 `.merge(crate::<mod>::routes())` 模式的证据，但 `routes.rs` 本身是一个与 Axum `Router` 状态树绑定的模块注册中心。实际上，这意味着：
   - 添加新端点也会触及这个文件（合并冲突磁铁）
   - 模块级路由（例如 `bookmarks::routes()`、`search::routes()`）可以更好地分布在子路由器中
   - 健康/就绪性/指标是单独的文件，但主要的 API 路由都落在这里

**2. Hub 的进程作用域导致问题。** `DashMap<ParticipantId, Vec<WsSender>>` 微妙地限制了操作模型：
   - 每个 `ParticipantId` 在同一进程中接收所有事件（即使他们在多台计算机上登录——不过这种情况很少见）
   - 跨实例扇出通过 NATS 进行（这是正确的），但进程间的 `ParticipantId → process` 绑定是隐式的（NATS 消费者在 `im.room.*` 上，每个进程去重）。这意味着如果同一个用户连接到进程 A 和 B，他们会在两个进程上收到重复的事件。*如果*这被 NATS consumer 的分发语义所解决，我需要检查。

**3. 缺少优雅的关闭协调。** 看门狗/定时器从 `background.rs` 生成，进程级别的 `CancellationToken` 用于关闭，但 crate 边界没有定义关闭序列。如果一个 AI 作业在 `ai_jobs` 轮询循环中被中止，`AiWorker` 会通过 `DROP TABLE IF EXISTS` 重新排队吗？这份文档（AGENTS.md）说「死信 `MAX_ATTEMPTS=5`→`dead`」，但如果在 `spend` 途中一个 worker 被杀死，重新调度之间的差距是 at-least-once 的。

**4. Web SPA 是一个单体「调试客户端」。** 在 5939 行生产 UI 代码中，`app.js` 是 1009 行（事件调度、选项卡管理、导航），`render.js` 是 685 行（将状态树转换为 DOM 的 `innerHTML` 集）。没有 CSS 模块化（`style.css` 是 1236 行全局）；没有测试；没有 bundler（尽管有 19 个模块，但通过原始 `<script type="module">` 加载）。验证分析文档中提到的「19 次往返」是准确的——每个模块都是一个单独的 HTTP 请求。

**5. SFU 媒体层是可测试但未连接的。** 「媒体接缝」部分（`sfu_media.rs`、`call_bridge_supervisor.rs`）具有干净的接口和单元测试，但 `run()` 循环在测试之外从未被调用。这是一个基础设施接缝，不是死代码——但这意味着完整通话系统的端到端验证必须手动完成，使用真正的浏览器。

### 架构债务

| 类型 | 位置 | 影响 |
|------|------|--------|
| **多租户缺失** | Redis 键没有命名空间；blob 路径没有 workspace 前缀；连接池是全局的 | 不能将不同客户的数据存放在同一个 PG 模式/Redis 实例中 |
| **前端零测试** | `web/` — 0 个测试文件 | 后端的每一次重构都可能无意中破坏 SPA；没有回归安全网 |
| **入口文件巨大** | `routes.rs` (2854 行) | 每个新端点都是一个碰撞风险；合并是手动的 |
| **环境配置传播** | `AERO__DOUBLE__UNDERSCORE` vs `AERO_SINGLE_UNDERSCORE` 没有明确的 enum | 新贡献者经常搞混；在配置键上 grep 会产生误报 |
| **n+1 查询风险** | `NotifyBatch` 展开（在 `bus.rs` 中）——通过 NATS 进行 O(N) 显式收件人展开 | 一个有 100 个成员的房间会向 NATS 发布 100 条消息 |

---

## 2. 扩展方向

### 方向 1：租户隔离基础设施（P0 — 商业化前提）

**为什么需要：** 没有租户隔离，Enterprise 客户无法通过安全审计。它不是「以后的功能」——它是合同要求。当前系统假设一个逻辑租户：所有房间共享一个 workspace 名称空间，一个 Redis 键空间，一个 blob 存储桶。

**核心挑战：**
- 跨 15-20 个硬编码键模式的 Redis 键迁移（presence、stream_viewers、call_rosters 等）
- 不带 workspace 前缀的遗留 blob 的 Blob 存储回退
- 连接池耗尽（每个租户的最小连接数 × 租户数 × 峰值并发）
- 不带 workspace 上下文的现有 SQL 查询（`WHERE room_id = $1`，但 `rooms.workspace_id` 已经存在）

**预期的架构变更：**
```
当前：Redis key = format!("presence:room:{room_id}")
未来：Redis key = format!("ws:{workspace_id}:presence:room:{room_id}")
      回退：如果不匹配旧前缀则尝试不带 ws: 前缀的键（零停机迁移）

当前：PgPool = 一个池适用于所有查询
未来：TenantPoolManager 具有：
  - PgPool 每租户（min:2, max:8）结合全局共享池用于跨租户操作
  - 在 AppState 中，带有 `resolve(workspace_id) -> PgPool` 方法的 trait

当前：blob_store.get(blob_id)
未来：TenantAwareBlobStore 包装器：
  - 写入：store(ws_id/blob_id, data)
  - 读取：get(ws_id/blob_id)，回退到 get(blob_id)
```

**对现有系统的影响：**
- 中等。`aero-storage/src/lib.rs` 中的存储库目前直接获取 `PgPool`。`TenantPoolManager` 的注入需要一个 trait bound，但大多数查询已经是 room-scoped 或 workspace-scoped
- 高影响但易 grep：Redis 键模式——用新的格式化程序 grep 替换 `format!(...)` 模式
- 低影响：Blob 存储包装器——`BlobStore` trait 已经存在，只需一个新 impl

**三个选项的权衡：**
| 选项 | 成本 | 隔离 | 运营 |
|--------|------|------------|-------------|
| A: 每租户连接池 | 中等（每租户 ~100 行样板 + pool 管理） | 好（PG 连接、Redis 键空间） | 简单（每个池独立配置） |
| B: 每租户模式 | 高（在 N 个模式上迁移，跨租户查询通过 `search_path`） | 最佳（完全表级隔离） | 中等（迁移管理，备份） |
| C: 每租户数据库 | 非常高（N 个 PG 实例，连接路由） | 最大 | 复杂（编排、备份、HA） |

**建议：** 先做 A（2 周），如果需要更强隔离则评估 B。

---

### 方向 2：前端生产化（P0 — 用户感知）

**为什么需要：** 在当今的市场中，UI 是产品。一个加载有 19 个独立模块的单体 `innerHTML` SPA 会产生可感知的延迟（`navigator.hardwareConcurrency` 在慢速设备上会很吃力）、没有错误边界，并且每次 DOM 更新时都会破坏状态。后端功能矩阵毫无意义，除非用户在点击「发送」后能看到消息出现。

**核心挑战：**
- 在没有测试安全网的情况下，将 5939 行原生 DOM 操作重写为组件化架构
- 维护与当前 API 契约的向后兼容性（WS 帧类型、REST 端点）
- 在当前代码中提取并保留「微妙的不变量」。例如，`render.js` `.textContent =` 与 `innerHTML =` 的混合意味着 XSS 防护模式很脆弱
- 19 次 HTTP 往返需要在 bundler 代码拆分中进行资产管理

**预期的架构变更：**
```
当前：index.html 具有 19 个 <script type="module" src="*.js"> 标签
未来：Vite 入口 + 代码拆分：
  - 主要：app.js（导航、外壳）
  - 延迟：calls.js（仅当有来电时加载）
  - 延迟：live.js（仅当加入直播时加载）

当前：render.js 具有 innerHTML 集
未来：具有虚拟 DOM 差异的组件化渲染（Preact 或 Lit）
  - 每个 UI 组件一个文件
  - 道具 → 视图 纯函数
  - 引导时水合，非 SSR

当前：0 个测试
未来：vitest 用于：
  - 渲染函数输出（纯函数测试）
  - WS 帧处理程序（mock WebSocket）
  - API 调用（mock fetch）
```

**对现有系统的影响：**
- 低到中等。后端 API 不需要改变（契约保持不变）
- 开发服务器需要 Vite 代理（`/api/*`、`/ws` → 后端）
- 风险：在迁移完成之前，旧的 SPA 需要在旁边可用（例如，`/admin/debug.html` 保留旧 UI 以进行 API 调试）

**框架选择：**
| 框架 | 包大小 | 学习曲线 | 迁移路径 |
|----------|-----------|-------------|----------------|
| Preact | ~3KB | 低（React API） | 中等（逐组件替换 `innerHTML`） |
| Lit | ~5KB | 中（Web 组件） | 低（保留原生 DOM 操作模式） |
| Svelte | ~0KB 运行时 | 中（新语法） | 高（编译时，需要构建步骤） |
| 纯 WC | 0KB | 高（需要显式生命周期） | 中等（没有框架锁定） |

**建议：** Preact。最小的 API 差异；可以逐步采用（一个组件一个组件的）；React 生态系统的测试工具。

---

### 方向 3：特性标志 + 配置热重载（P1 — 运营安全网）

**为什么需要：** 该文档说对了：「没有运行时配置和特性标志的生产系统，每一次部署都是一场赌博。」特别是随着租户隔离的进行（方向 1），每个租户都需要独立的功能推出。当前系统需要重新启动才能更改任何配置值。
环境变量（`AERO__*`、`AERO_*`）在启动时全部读取并结构化为 `AppConfig`。

**核心挑战：**
- 识别哪些配置值是「安全的」用于热重载与哪些需要重新启动（TLS 证书、数据库 URL、监听地址）
- 使 config 的 figment 层支持重新加载（目前它是启动时冻结的）
- 在 Axum 路由处理程序的热路径中避免标志检查开销（每个请求都需要原子/缓存查找）
- 标志膨胀治理（标志只增不减）

**预期的架构变更：**
```
当前：let config = AppConfig::from_env() 在 main() 中
未来：
  - RuntimeConfig 包含可重载的值（rate_limits、feature_flags、AI 端点）
  - SIGHUP 处理程序重新加载 RuntimeConfig 而不重新启动服务器
  - FeatureFlagStore（Redis + PG 持久化）用于每工作区标志
  - 本地缓存（30 秒 TTL）避免每个请求都访问 Redis

设计原则：
  struct FeatureFlag {
      name: String,
      enabled: bool,
      scope: FlagScope,  // Global | Workspace | User
      created_at: DateTime,
      expected_removal_at: Option<DateTime>,  // 强制治理
  }
```

**对现有系统的影响：**
- 低。添加一个新配置存储，无需更改现有配置加载。
- 在启动时添加一个注册步骤，将热重载值绑定到信号处理程序。
- Cargo 特性不会改变——这是运行时，不是编译时。

**实施顺序：**
1. Phase A（1 天）：SIGHUP 热重载 ~10 个配置值（限流、AI 端点、日志级别）
2. Phase B（2 周）：使用 Admin API 和 Redis 持久化的完整 Feature Flag 系统
3. Phase C（持续）：每季度标志清理扫描

---

### 方向 4：通话媒体可观测性与弹性（P1 — 通话质量保证）

**为什么需要：** WebRTC 在 5 分钟内从「完美工作」变成「没有音频」，中间没有任何东西。没有 `getStats()` 轮询，没有 ICE 重启，没有连接质量指标——当前系统的 `CallOrchestrator` 覆盖了信令层，但媒体层是一个黑匣子。用户会说「通话卡顿」，你没有任何数据来调试。

**核心挑战：**
- 多方 SFU 通话中的 ICE 重启并非易事（所有订阅者都必须同步切换）
- `getStats()` 轮询会产生大量数据（每 5 秒每个对等连接）；需要汇总和阈值
- 通话后指标必须持久化到 `call_stats` 表，并具有有意义的保留策略
- 实时字幕需要 WS 推送路径，该路径从 `AiWorker.transcribe` 连接到通话中的流

**预期的架构变更：**
```
当前：CallOrchestrator → 信令事件
     SfuMediaSession → on_rtp → forwarder（纯媒体，无指标收集）
未来：
  客户端：每 5 秒 getStats() 采样 → 发送到 {type:"call_stats", ...}
  服务器：CallStatsCollector
    - 汇总 RTT/丢包率/抖动/带宽/帧率
    - 计算通话质量评分（1-5 分，类似 MOS）
    - 持久化到 call_stats 表
    - 如果指标低于阈值则触发告警

  ICE 重启流程：
    1. 客户端 createOffer({ iceRestart: true })
    2. 服务器 SfuMediaSession 接受新候选者
    3. 所有订阅者同步更新 ICE 连接
    4. 重启期间的媒体丢失在日志中计数

  实时字幕：
    AiWorker.transcribe → {type:"call_caption", text, timestamp}
    → hub.call_subtitles → WS 扇出
```

**对现有系统的影响：**
- 中等。`SfuMediaSession` 需要一个新的 `on_stats` 回调或轮询机制。
- `CallOrchestrator` 需要一个新的 `record_call_stats` 方法。
- WS 帧枚举增长了一个 `CallStats` / `CallCaption` variant。
- 客户端需要 WebRTC `getStats()` API 支持（所有现代浏览器都有）。

**与前端工作的关系：**
- 通话 UI（录制按钮、质量指示器、字幕显示）完全属于方向 2 的 SPA 重写
- 方向 4 应优先完成后端指标收集和 ICE 重启逻辑，然后前端在方向 2 完成后再接入 UI

---

### 方向 5：基准 + 负载测试（P0 — 所有决策的前提条件）

**为什么需要：** 该分析文档说对了——没有基线数据，你就无法知道你的 1500 行架构重构是让系统变快还是变慢。此外，客户会在合同前要求「支持多少并发用户？」

**核心挑战：**
- 全链路负载测试需要真实的 PG、Redis、NATS 实例（比单元测试资源密集得多）
- 可重现的结果需要专用的测试环境（开发/共享数据库上的随机波动会使数字失效）
- WS 并发基准测试需要 k6 脚本，这些脚本会生成用户、发送消息并验证扇出
- SFU 基准测试需要生成 RTP 包，而不是浏览器（无头浏览器太重了）

**预期的架构变更：**
- 无。这是用新的基准测试/测试代码添加测试基础设施，而不是更改生产代码
- Criterion.rs 基准测试用于组件级指标（消息序列化、RTP 重映射、DB 查询延迟）
- k6 脚本用于全链路场景（并发 WS 连接、消息吞吐量、AI 搜索延迟）

```
基准测试套件结构：
crates/aero-server/benches/
  message_throughput.rs    — 端到端：WS → Hub → NATS → DB → fan-out → WS
  ws_concurrency.rs        — 单进程最大 WS 连接数
  db_query_scaling.rs      — list_since 延迟与房间中的消息数
crates/aero-live-webrtc/benches/
  sfu_forwarding.rs        — RTP 包转发速率（无 str0m 握手开销）
crates/aero-ai/benches/
  embedding_latency.rs     — 嵌入生成吞吐量
k6/
  scenarios/
    chat_load.js           — 恒定 VU 消息发送
    search_load.js         — 混合搜索查询
    ai_questions.js        — AI 问答并发
```

**测试环境策略：**
- CI：仅组件级基准测试（`cargo bench`）——不需要外部服务
- 每周：在专用的 throwaway 环境中进行全链路 k6 运行（重用 `DATABASE_URL` 门控模式）
- SFU：仅限本地开发者（需要专用网络设置）

---

## 3. 接口设计建议

### 需要新的抽象层

**1. `TenantAware` trait 用于跨层资源检索**

从未来架构的角度来看，最大的痛点是没有通过抽象来统一 Redis 键模式、数据库连接路由和 blob 路径。每个 crate 目前都会自行为其 Redis/presence 键构造字符串。

```rust
/// 一个跨层 trait，供需要租户感知资源访问的组件实现。
/// 目前还不是一个具体 trait——而是跨 crate 的一种*模式*。
///
/// 消费方（存储库、总线监听器、hub）调用带有 workspace_id 的 resolve，
/// 管理器则路由到正确的连接池/键空间/存储桶前缀。
trait TenantContext {
    fn workspace_id(&self) -> Option<WorkspaceId>;
    fn pg_pool(&self) -> &PgPool;  // 如果每个租户一个池，则解析正确的池
    fn redis_key(&self, suffix: &str) -> String;
    fn blob_path(&self, blob_id: &BlobId) -> String;
}
```

这直接解决了「15-20 个硬编码 Redis 键格式」的问题，而没有引入新的运行时依赖。在迁移期间，当 `workspace_id` 为 `None` 时，实现回退到旧键模式。

**2. `FeatureFlagStore` 用于运行时功能门控**

当前系统没有每个租户功能的门控概念。添加一个具有以下特征的存储：

```rust
#[async_trait]
trait FeatureFlagStore: Send + Sync {
    /// 向调用者返回给定范围的标志值。
    /// 期望返回值被本地缓存约 30 秒。
    async fn is_enabled(&self, flag: &str, scope: FlagScope) -> bool;

    /// 设置一个标志值，可选择设置 TTL。
    async fn set(&self, flag: &str, scope: FlagScope, enabled: bool, ttl: Option<Duration>) -> Result<()>;

    /// 列出所有活跃标志（用于治理扫描）。
    async fn list_active(&self) -> Result<Vec<FeatureFlag>>;
}
```

Redis 后备加上 PG 持久化是合适的。不需要 etcd——`watch` 语义对每日几次的变化来说是大材小用。

**3. `CallMetricsCollector` 用于桥接 WebRTC stats → 持久化**

目前媒体是哑转发——没有指标总结。WebRTC 规范没有为 stats 定义服务器端 API（RTCP 发送方/接收方报告是最接近的），因此收集必须在客户端完成。需要新的接口：

```rust
#[async_trait]
trait CallMetricsCollector: Send + Sync {
    /// 用客户端报告的轮询结果更新通话指标。
    async fn record_stats(&self, call_id: CallId, participant_id: ParticipantId, stats: CallStatsSnapshot);

    /// 获取给定通话的汇总质量评分。
    async fn quality_score(&self, call_id: CallId) -> Option<f32>;

    /// 归档通话结束时的指标（用于通话后审查）。
    async fn finalize_call(&self, call_id: CallId) -> Result<CallQualityReport>;
}
```

### 冗余抽象可以移除

**避免过早抽象的领域：**
- **通用的「消息队列」trait**，位于 NATS 之上。`EventBus` trait 足够好；JetStream 的具体细节已经被很好地封装了。支持 Kafka 将是一个很大的努力，但这是假设性的。
- **Axum 之上的「通用 HTTP 框架」**。当前的 Axum 路由模式（带有 `AuthUser` 提取器的 `Router<AppState>`）是惯用的，并且工作良好。添加自定义提取器包装器（`AuthenticatedRoomRoute`）将增加样板代码，而收益甚微。
- **「通用 SFU」trait**，位于 `str0m` 之上。当前的 SFU 与 str0m 的 `Rtc` 模型紧密耦合，这是有意的——str0m 是一个 Rust-native 选择，替换它将是一个完全的重写。

### 向后兼容性

**对于方向 1（租户隔离）：**
- Redis 键迁移需要双读：检查 `ws:{id}:*`，然后检查旧键
- 遗留 blob 需要双读：检查 `{workspace_id}/{blob_id}`，然后检查 `{blob_id}`
- PG 连接池可以逐步添加：首先使用共享池作为后备，然后逐个租户地迁移

**对于方向 2（前端）：**
- 当前的 WS 帧类型不会改变（`ServerFrame`/`ClientFrame` 枚举保持不变）
- REST API URL 不会改变（`/api/rooms/:id/messages` 等）
- 可以保留遗留的 `index.html`，作为新 SPA 旁边的调试入口（例如，`/admin/debug.html`）

**对于方向 3（特性标志）：**
- 没有标志的当前行为就是默认行为（标志默认为 `false`/启用）
- 标志清理是添加性的

---

## 4. 技术选型

### 需要什么新的依赖？

| 组件 | 候选 | 理由 |
|-----------|---------|---------|
| JS bundler | **Vite** | 已经是最小配置；原生 ESM 支持；微不足道的学习曲线 |
| JS 测试框架 | **vitest** | 与 Vite 共享配置；与 Jest 兼容的 API；快速 |
| JS 框架 | **Preact** | 类似于 React 的 API；3KB；最小迁移路径 |
| 负载测试 | **k6** | 原生 WS 支持；CI 友好；JavaScript 脚本 |
| Rust 基准测试 | **Criterion.rs** | 已经在 Cargo.toml 中提到；CI 集成；历史回归检测 |
| 通话指标 | **无新的** | `getStats()` 是浏览器 API；在后端进行 JSON 收集和汇总 |
| 特性标志 | **Redis Hash + PG** | 已经部署了 Redis 和 PG；不需要新的持久化层 |

### 不要引入

| 技术 | 为什么不需要 |
|----------|----------------|
| etcd | 运维复杂性，仅用于特性标志（~0.1 QPS）是大材小用 |
| React | 17KB 最小包大小 vs Preact 的 3KB；对于 5.9K 行代码库来说，迁移路径更长 |
| Webpack | Vite 优于 Webpack 的开发/生产体验；原生 ESM 支持消除了对 polyfill 的需求 |
| TypeScript | 在当前阶段增加了编译步骤和类型复杂性；JSDoc 注释可以提供类型提示，无需 `tsc` |
| gRPC | 系统是 HTTP+REST+WS；gRPC 将增加 protobuf 生成和网关层 |
| Kafka | NATS 满足当前吞吐量需求；Kafka 增加了显著的运维复杂性 |

### 自建 vs 采购

| 决策 | 自建 | 采购 | 理由 |
|--------|------|------|---------|
| 特性标志系统 | ✓ | | Redis + PG 模式足够简单；现有的租户系统不需要 SaaS 功能标志后端 |
| 通话质量仪表板 | ✓ | | 指标收集部分是基础设施（需要服务器端）；UI 适合现有的 SPA |
| 负载测试基础设施 | ✓ | | k6 脚本是自包含的；不需要 Loader.io / Flood.io |
| SSO/OIDC | ✓（已经存在） | | `aero-auth` 已经有了 OIDC JIT；不需要 Auth0 / Okta |
| 推送通知 | ✓（已经存在） | | `aero-push` 已经有 FCM/APNs 网关；不需要 Firebase Cloud Messaging 包装器 |
| 前端组件库 | | ✓ | 不要自建日期选择器、颜色选择器、文本编辑器——使用 Shoelace（Web 组件）或 CDN 库 |

---

## 5. 实施路线图

### 优先级排序

```
P0 ─── 方向 5：负载测试（先获取基线数据）
   └── 没有数据，每个设计决策都是在猜
P0 ─── 方向 2 Phase A：JSDoc + vitest 覆盖率（前端最低安全性）
   └── 没有测试，每一次前端重构都是盲飞
P0 ─── 方向 1 Phase A：每租户连接池 + Redis 键命名空间（商业化前提）
   └── 没有租户隔离，企业客户无法签约
P1 ─── 方向 3 Phase A：SIGHUP 热重载（基础设施快速制胜）
   └── 3 天的实施成本，消除 80% 的「改配置=重启」痛苦
P1 ─── 方向 4 Phase A：通话质量指标收集（可观测性基础）
   └── 没有指标，通话性能问题无法可重复调试
P1 ─── 方向 2 Phase B：Vite + CSS 代码拆分（减少负载时间）
   └── 解决了最直接的 UX 痛点
P2 ─── 方向 3 Phase B：完整特性标志系统（每租户功能门控）
   └── 需要租户隔离基础设施已经在运行
P2 ─── 方向 1 Phase B：Blob 存储租户隔离 + 后台填充
   └── 依赖于每租户连接池的稳定
P2 ─── 方向 4 Phase B：ICE 重启 + 实时字幕
   └── 通话基础已经在工作；这些是受控的改进
P2 ─── 方向 2 Phase C：E2E 测试 + 可访问性（质量保证）
   └── 在组件化之后，测试更容易编写，可访问性是锦上添花
P3 ─── 方向 4 Phase C：通话录制 + 屏幕共享
   └── 依赖于 SPA 重写（方向 2）来提供 UI
```

### 阶段划分

**Phase A（周 1-2）：建立基线**
- 方向 5：Criterion.rs 组件基准测试 + k6 负载测试用于消息吞吐量
- 方向 2：JSDoc + vitest 设置 + 3 个核心渲染函数测试
- 方向 1：Redis 键 grep + 路径映射文档
- 方向 3：SIGHUP 热重载实现

**Phase B（周 3-5）：核心基础设施**
- 方向 1：每租户连接池 + Redis 键命名空间（迁移向）
- 方向 2：Vite 集成 + CSS 代码拆分 + 组件化 app.js
- 方向 4：客户端 `getStats()` 收集 + 服务器端汇总
- 方向 3：特性标志表 + Redis 缓存 + Admin API

**Phase C（周 6-8）：完整功能支持**
- 方向 4：ICE 重启（1:1） + 实时字幕
- 方向 2：E2E 测试 + 可访问性改进
- 方向 1：Blob 存储租户隔离 + 后台填充
- 方向 4 Phase C（如果方向 2 E2E 已经稳定）：通话录制 UI + 屏幕共享

**Phase D（周 9-10）：收紧 + 治理**
- 方向 3：特性标志季度清理扫描
- 方向 1：评估每租户模式的必要性
- 可选的基准测试数据看板（方向 5 的扩展）
- 全链路负载测试运行 + 性能调优

### 风险矩阵

| 风险 | 可能性 | 影响 | 缓解措施 |
|------|------|----------|--------------|
| SPA 重写破坏现有的「微妙不变量」 | 中 | 高 | Phase A 中维护 `admin/debug.html`；在重写之前添加捕获当前渲染输出的测试 |
| Redis 键迁移数据丢失（旧键到期，新键没有被写入） | 低 | 高 | 双读（新 → 旧 → 回退）；至少保留旧键的 TTL |
| 每租户连接池耗尽 PG `max_connections`（100 个租户 × 8 = 800） | 中 | 中 | 在池管理器中使用连接配额池；在超出 PG 限制之前进行压力测试 |
| ICE 重启在多方 SFU 通话中损坏（同时性错误） | 中 | 高 | Phase A 中首先为 1:1 通话实现；在 SFU 通话上启用特性标志；广泛的集成测试 |
| 特性标志膨胀（无治理） | 高 | 低 | 标志中的 `expected_removal_at` 字段；每季度 CI 扫描 |
| 负载测试环境成本（专用的 PG/Redis/NATS） | 低 | 低 | CI 中的组件级基准测试；每周全链路运行 |
| 团队在 SPA 中选择了错误的框架 | 中 | 中 | Preact 最小化锁定（API 与 React 兼容）；如果 Preact 表现不佳，Lit 是一条几乎免费的退出路径 |

---

## 最后的架构观察

该分析文档正确地识别了关键问题：系统在单租户场景中功能完备，但需要在三个维度上成熟才能进行商业化部署：

1. **隔离（多租户）**：目前是 1 个客户 × 所有功能。需要成为 N 个客户 × 所有功能，并附带每客户的数据和性能边界。

2. **可观察性（指标 + 负载测试）**：没有基线数据，性能调优是猜测。没有通话质量指标，WebRTC 问题无法调试。没有负载测试，扩展决策是基于理论的。

3. **交付（前端 + 特性标志）**：后端需要是可靠的；UI 需要是流畅的；发布需要是可控制的。目前三者都不存在。

代码库本身的质量很高——crate 边界、trait 设计、事件总线抽象、存储层冗余都是顶级的。生产就绪差距不在代码质量上，而在那些功能完备性和生产可交付性之间的跨领域问题上。
