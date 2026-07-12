以下是从架构师角度出发，基于验证报告和原分析文档的深度架构分析。

---

# 架构分析：Aero IM 战略扩展方向的技术评析

## 1. 架构评估

### 1.1 现有架构的优势

Aero IM 的核心骨架——**事件驱动 DAG + 进程内扇出 + 集群级 Redis 状态**——是一个经过深思熟虑的设计，在以下维度表现优异：

| 维度 | 评价 | 依据 |
|------|------|------|
| **事件溯源** | ★★★★★ | NATS JetStream durable consumer 作为事实源，at-least-once 语义 + 单调 seq 天然支持去重/排序 |
| **水平扩展性** | ★★★★☆ | Hub 是进程内 `mpsc`，无共享扇出状态；多开实例即水平扩，仅 Redis 为集群级瓶颈 |
| **模块隔离** | ★★★★★ | Feature-first crate 组织（`aero-im-core`、`aero-live-webrtc`、`aero-push` 等）依赖方向明确，不成环 |
| **部署确定性** | ★★★★★ | 迁移编译期嵌入 `sqlx::migrate!("../../migrations")`，运行时无数据库版本漂移 |
| **存储分离** | ★★★★★ | PG = 持久事实，Redis sorted-set = 集群临时状态（presence/roster/viewer），角色清晰 |
| **类型安全** | ★★★★★ | Rust 强类型 + tagged enum（`RoomEvent`/`StreamEvent`）的 `kind` 标签 + JSON 序列化，前后端契约明确 |

### 1.2 关键局限

从架构演进的角度看，以下局限决定了哪些方向是高价值扩展：

**① 无客户端抽象层（最根本的架构缺口）**

当前 Web SPA 的代码（`web/{app,ws,api}.js`）是**面向 DOM 的脚本而非面向协议的 SDK**。`WsClient`、`SeqGate`、消息缓存逻辑都与浏览器全局变量（`window.chatCache`、`window.ws`）紧耦合。这意味着：

- 无法在不重写的前提下在 React Native / Flutter 中复用
- 无法做 Service Worker 的 message relay（DOM 操作在 SW 上下文中不可用）
- 测试只能通过 `puppeteer` 端到端跑，无 unit-testable 的协议层

**决策原点**：项目初期优先快速产品验证，选择了零依赖 SPA + 最小 JS。这是正确的权衡——但进入战略扩展阶段，**这个约束需要解除**。

**② Hub 的单体结构正在膨胀**

`Hub` 结构体（`hub.rs:114-122`）在一个 struct 中聚合了 5 个 `DashMap`（`conns`, `rooms`, `stream_watchers`, `call_rosters`, `subs`）。项目初期这是一种"集中管理"的合理选择——但要支持方向四（多集群联邦），需要区分「本 Hub 的本地连接」和「远程实例的代理连接」；要支持方向五（反滥用升级），需要在线程内注册每个 WS 连接的信誉数据。**本质上是 `Hub` 的职责正在从"连接管理"向"实时会话上下文"演化。**

**③ 搜索数据管道有"生产端"无"消费端"**

迁移 0133 `search_click_events` 表 + `SearchFeedbackRepo::record_click` + `ctr_stats()` 已经是一条完整的数据管道——但它的输出只停留在存储层，从未被 `merge_hits` 排序函数消费。这是架构层面的**不完整抽象**：数据收集是昂贵的（每次 click 一次 INSERT），如果数据不用就是纯负债（存储 + 写入放大）。

**④ Redis 一致性假设未显式建模**

当前系统假设 Redis 是可靠的单点（单实例或主从）。但：
- `ws_rate.rs` 的固定窗口限流依赖 Redis 原子计数，主从切换时可能翻倍
- `live_presence` 的 viewer count 依赖于 `ZADD` + `ZREMRANGEBYSCORE` 心跳，网络分区时计数漂移
- 这些没有在架构层面建模为"尽力而为的一致（eventually consistent）"——可能导致运维时对计数准确性的错误预期

### 1.3 架构债务汇总

| 债务项 | 严重程度 | 触发条件 | 建议修复时机 |
|--------|---------|---------|------------|
| Tagged enum `kind` 字段冲突需 `serde(rename)` 规避 | 🟡 中 | 新加 `RoomEvent` variant 时隐形坑 | 每次加新 variant 时保持警惕，方向二/三/四加新 event 时尤为注意 |
| Token helper 同名且在 crate root 有条件 re-export | 🟢 低 | 新加 `generate_token` 函数时易撞名 | 维持当前约定（只在 `webhook` 模块 re-export），写进 AGENTS.md 4.2 |
| 限流两套并行实现（token bucket + Redis window）语义不一致 | 🟡 中 | 分布式部署时 bucket 本地状态 vs Redis 全局状态可能偏差 | 方向五的滑动窗口重构可统一 |
| Click 数据采集但未消费 | 🟡 中 | 无用存储 + 运维人员误以为"搜索质量支持已就绪" | 方向三 P0 接入排序 |

---

## 2. 扩展方向

以下扩展方向基于验证报告确认的 5 个方向，但从架构师视角重新排序和深化。

### 方向 A：客户端协议抽象层（P0 · 前置基础）

**为什么需要**：这是方向一（Offline-First/PWA）和方向二（原生移动端）的共同前置条件。没有协议抽象层，PWA 的 Service Worker 无法复用 WebSocket 逻辑，原生端也无法复用协议状态机。

**核心挑战**：

1. **从"DOM 脚本"到"协议库"的分解**：需要识别 `ws.js` 中的领域逻辑（连接管理、心跳、seq gate、重连 backoff、消息序列化/反序列化）与 UI 逻辑（渲染、滚动、DOM 操作），提取为无平台的 `ProtocolClient`。
2. **平台适配器模式**：`ProtocolClient` 不直接操作 `WebSocket`、`fetch`、`Notification`、`indexedDB`——它定义 trait/interface，各平台注入适配器：
   - Web: `WebSocket` + `CacheStorage` + `Notification API`
   - Service Worker: `Clients.matchAll()` + `ExtendableMessageEvent`
   - React Native: `react-native-webrtc` DataChannel + `AsyncStorage`
3. **流式兼容**：现有 Web SPA 必须在同一代码仓内继续工作——不能一次性重写。需要提供"渐进式包裹"策略：新抽象层在 `web/protocol/` 下开发，通过适配器调用旧的 `ws.js` 全局函数，逐步替换。

**预期的架构变更**：

```
web/
├── protocol/                  # 新：平台无关协议层
│   ├── client.rs / ts         # ProtocolClient: connect, send, receive, reconnect
│   ├── seq_gate.ts            # 从 ws.js 分解出来
│   ├── heartbeat.ts
│   ├── frames.ts              # ClientFrame / ServerFrame 类型定义
│   └── adapters/
│       ├── web-socket.ts      # Web WS adapter
│       ├── sw-message.ts      # SW message relay adapter
│       └── mock.ts            # 测试用 mock adapter
├── app.js (逐步瘦身)          # 旧 UI 层，改为 import protocol
├── sw.js (新)                 # Service Worker
└── ws.js (逐步废弃)           # 旧协议逻辑合并入 protocol/
```

**对现有系统的影响**：低。新文件 + 逐步替换，现有 SPA 功能不受影响。`SeqGate` 去重逻辑的接口保持不变，只是从 `ws.js` 提取到 `protocol/seq_gate.ts`。

---

### 方向 B：离线优先数据层（P0 · 用户体验驱动）

**为什么需要**：验证报告确认了最关键的缺口——无 Service Worker、无 IndexedDB 缓存、无离线发件箱。对于一个 IM 产品，这是产品级与演示级的根本分界线。

但架构上不能简单地在客户端"加个 cache"——需要设计**离线数据一致性模型**：

**核心挑战**：

1. **消息的最终顺序**：离线期间收到的消息（通过 `?since=` 回填）和本地离线发送的消息合并时，ULID 保证了全局有序——但编辑/删除/反应这类更新操作必须在消息已缓存之后才能应用。离线存储需要支持"pending mutation queue"。
2. **乐观 UI 与离线发件箱的冲突**：现有 `optimisticAdd` 在发送前就渲染消息。如果这条消息是在离线时发的，随后 WS 重连后服务端返回的消息 seq 可能与本地临时 ID 冲突。需要一个**临时 ULID → 最终 ULID 的映射层**。
3. **存储配额管理**：IndexedDB 在移动端 Safari 上可能只有 50MB 配额。需要 LRU 淘汰 + 按房间分级缓存（活跃房间全缓存，归档房间仅索引）。

**架构方案选型**：

| 方案 | 一致性模型 | 复杂度 | 离线写支持 | 推荐 |
|------|-----------|--------|-----------|------|
| 简单 `?since=` 缓存 | 最终一致 | ★☆☆ | ❌ | 过渡期 P0（2周） |
| CRDT (Automerge/Yjs) | 强最终一致 | ★★★★★ | ✅ | 中期 P2（6月+） |
| 操作日志 + 重放 | 最终一致 | ★★★☆ | ✅ | **推荐 P1** |

**推荐路径**：先做简单缓存（P0，纯读缓存），再升级为操作日志（P1，支持离线发件箱）。CRDT 虽强大但其同步协议与现有 `RoomEvent` 架构的兼容性分析需独立投入——不建议纳入初期范围。

**关键架构决策**：离线发件箱是否应该在 Service Worker 中运行？

- **SW 中运行**：可以跨标签页维护离线队列、在后台同步；但 SW 生命周期不可控（浏览器随时 kill SW），不适合长连接维护
- **页面中运行**：生命周期可控，但关闭标签页后队列丢失
- **推荐**：双轨——页面存活时由 `ProtocolClient` 管理离线队列；页面关闭时 SW 只负责最后一条消息的暂存 + `SyncManager` 注册

---

### 方向 C：多集群联邦的数据层设计（P1 · 企业价值驱动）

**验证报告确认**：当前架构是单集群的，这是企业客户硬需求。但验证报告也确认了 `call_bridge` 架构已就绪、`Hub` 是进程内 DashMap——这些意味着**联邦架构的核心假设已经部分内置**。

**核心挑战（按难度排序）**：

1. **身份联邦**（P0）：集群 A 的用户如何出现在集群 B？最可行的方案是**JWT 交换 + 访客身份**：
   - 集群 A 生成跨集群 JWT（含 `participant_id`、`workspace_id`、`role`），用集群 A 的私钥签名
   - 集群 B 缓存集群 A 的公钥，验证 JWT 后创建本地访客记录（`is_federated_guest = true`）
   - 访客权限：读本集群公开 room、发送受限（仅文字消息、无文件上传）
   - 这不需要改数据库 schema——`participants` 表加 `federated_from` 列即可

2. **消息桥接**（P1）：Room 的事件如何在集群间同步？
   - **方案 A（NATS Gateway）**：利用 NATS 原生 Gateway 功能桥接 `im.room.*` subject。延迟最低、运维最简。但要求两个集群的 NATS 网络互通。
   - **方案 B（HTTP Relay）**：自建 `room_bridge` 组件，订阅集群 A 的 `im.room.*` 并通过 HTTP POST 推送到集群 B 的 `/api/internal/bridge/events` 端点。网络隔离更强（适合跨公网），但延迟更高。
   - **推荐**：内部部署用方案 A，跨公网/跨云用方案 B。两种模式抽象为同一个 `BridgeTransport` trait。

3. **删除一致性**（P2）：集群 A 的用户删了一条消息，集群 B 的缓存怎么办？
   - 最简单：软删事件通过桥接通道同步（同 `Deleted` RoomEvent），**不作为硬删除**——集群 B 可安全忽略（消息从集群 A 删除不影响集群 B 已有的索引）
   - 更严格：主集群标记 `deleted_at`，桥接通道同步后集群 B 软删本地副本
   - **合规硬删除**：GDPR 数据删除请求需要跨集群级联→需要 `BridgeTransport` 支持"级联删除"类型

**对现有架构的影响**：

```
当前: Client → Hub(本地) → NATS → DB
联邦: Client → Hub(本地) → NATS → Gateway → 远端NATS → 远端Hub → 远端DB
                                       ↑
                              BridgeTransport trait
                              ├── NatsGatewayTransport
                              └── HttpRelayTransport
```

- `Hub` 需要区分本地 `WsSender` 和远程桥接 `RemoteSubscriber`（trait）
- `ImService::publish_room_event` 需要检查 room 是否跨集群（`room.federated_clusters`），如果是则通过 `BridgeTransport` 发布到远端
- 添加 `migrations/NNNN_federated_rooms.sql`：`rooms.federated_config JSONB`（`{clusters: ["eu-1", "us-1"], sync_mode: "mirror"}`）

---

### 方向 D：搜索排序的插件化架构（P1 · 数据驱动）

**验证报告的发现**：click 追踪已实现但未用于排序。这指向一个更大的架构问题——**搜索排名的扩展点未设计**。

当前 `merge_hits` 是静态的等权融合——新增一个排序信号（如点击率、回复数、作者权重）需要改核心代码。这不可持续。

**推荐的架构变更**：引入**排序特征管线（Ranking Feature Pipeline）**：

```
Search Query
    ↓
Retrieval (FTS / vector / hybrid)
    ↓
Feature Computation (可插拔)
    ├── TextRelevanceFeature    (ts_rank / 向量距离)
    ├── RecencyFeature          (now() - created_at 衰减)
    ├── ClickThroughFeature     (CTR from search_click_events) ← 现有数据
    ├── AuthorTrustFeature      (发送者的信誉分)              ← 与方向五接口对接
    ├── ThreadHeatFeature       (回复数/反应数/最近活跃)
    └── PersonalizationFeature  (用户常搜词/常回复作者)       ← 新
    ↓
Weighted Fusion (权重配置化)
    ↓
Final Ranking
```

**核心设计决策**：

1. **运行时 vs 编译时特征注册**：Rust 生态下编译时注册（trait + typemap）更惯用，但特征调整需要改代码。推荐**混合模式**：
   - 基础特征（FTS/向量/时间）编译时注册
   - 权重和启用/禁用由配置文件控制（`search_ranking.toml`）
   - 高阶特征（个性化/信誉）通过 `AiWorker` 异步计算、缓存到 `search_features` 表
2. **在线 vs 离线特征**：点击率 CTR 是实时特征（每个 click 写入 → 下次搜索即可见）。作者信誉是准实时（T+15min 更新）。离线 LTR 模型是 T+1 天。三者需要不同的刷新生命周期——用 refresh interval 标注。
3. **A/B 测试支持**：排序引擎应支持"实验配置"（如 variant="control" vs variant="ltr-v1"），通过响应头 `X-Search-Variant` 输出——方便前端埋点区分。

**对搜索响应时间的影响**：特征计算不能线性延长搜索延迟。设计约束：**所有特征计算共享 50ms 预算**，超时则降级（跳过该特征，用默认权重）。这意味着 `ClickThroughFeature` 必须走 Redis 缓存而非 PG 聚合查询。

---

### 方向 E：反滥用信誉层的架构嵌入（P2 · 运营安全）

**验证报告确认**：当前限流体系是全的但缺**信誉层**——这是一个架构层缺失而非功能缺失。

**核心架构问题**：信誉信息需要在多个子系统间共享——限流、AI 审核、举报处理、搜索排序、通话准入——但目前没有一个统一的"参与者上下文"结构传递这些数据。

**推荐的架构变更**：引入 `ParticipantContext` 作为请求处理管线的**环境对象**：

```
当前: AuthUser(participant_id, workspace_id, role)
      ↓ 每子系统独立查 redis/pg 获取信誉信息
      ↓ rate_limit.rs: token bucket（无信誉感知）
      ↓ moderation_bot.rs: KeyedCostBudget(60)（无信誉感知）
      
未来: AuthUser 扩展为 ParticipantContext
      │
      ├── identity: { id, workspace, role }
      ├── reputation: { score, tier, flags }
      │     └── 缓存来源: Redis hash (participants.reputation_* 热信号)
      │     └── 冷数据来源: pg participants.reputation_score
      ├── rate_limits: { per_sec, per_min, per_hour } (动态计算)
      └── abuse_signals: { is_suspicious_ip, is_fresh_account, has_verified_email }
```

**这个上下文对象在以下环节注入**：

| 环节 | 当前行为 | 加信誉后 |
|------|---------|---------|
| WS 连接建立 | `Hub::register` → 加入 conns | 计算初始信誉 → 分配限流档 + 标记可疑 |
| `sendMessage` handler | 走 token bucket | 上下文判断：低信誉 → 慢模式强制触发 + 验证码挑战 |
| `moderation_bot` | KeyedCostBudget(60) per-ws | 信誉加权预算：老用户 60 → 80，新用户 60 → 20 |
| `search_advanced` | 无信号 | 信誉词条 = 搜索结果降权因子 |
| `call_invite` | 无信号 | 低信誉用户被邀请时高亮提示 |

**集成点**：
- 当前 `AuthUser` 是 Axum `FromRequestParts` extractor——`ParticipantContext` 可以包装它，在 extract 后异步加载信誉缓存
- 缓存 TTL=60s（太短则 PG 查询压力大，太长则防不住批量注册后的快速滥用）
- 信誉更新事件（如用户被举报）通过 `bus/seq.rs` 管道通知各 Hub 实例刷新缓存

---

## 3. 接口设计建议

### 3.1 客户端 SDK 分层（构建方向 A 的基石）

```
┌─────────────────────────────────────────┐
│            Application Layer            │
│  (UI / DOM / React Native / Flutter)    │
├─────────────────────────────────────────┤
│         Presentation Adapter            │ ← 平台相关
│  (Web DOM / Native Widget / Notification)│
├─────────────────────────────────────────┤
│          ProtocolClient (核心)           │ ← 平台无关
│  Connect · Reconnect · SeqGate · Frames │
├─────────────────────────────────────────┤
│        Transport Adapter (抽象)          │ ← 接口隔离
│  WebSocket · Fetch · SW MessagePort    │
└─────────────────────────────────────────┘
```

**关键接口**：

```typescript
// Transport Adapter Interface（平台无关）
interface TransportAdapter {
  connect(url: string): Promise<void>;
  send(data: Uint8Array): void;
  onMessage(handler: (data: Uint8Array) => void): void;
  onClose(handler: (code: number, reason: string) => void): void;
  close(): void;
}

// Storage Adapter Interface（用于离线缓存）
interface StorageAdapter {
  getMessages(roomId: string, since: string, limit: number): Promise<StoredMessage[]>;
  putMessage(roomId: string, msg: StoredMessage): Promise<void>;
  getOutbox(): Promise<OutboxEntry[]>;
  enqueueOutbox(entry: OutboxEntry): Promise<void>;
  removeOutboxEntry(localId: string): Promise<void>;
}

// ProtocolClient 使用依赖注入
class ProtocolClient {
  constructor(
    private transport: TransportAdapter,
    private storage: StorageAdapter,
    private config: ClientConfig
  ) {}
}
```

这个设计的核心价值：**所有平台（Web / SW / React Native / Flutter）共享同一套协议逻辑**，只需各自实现 `TransportAdapter` + `StorageAdapter` + `PresentationAdapter`。

### 3.2 Ranking Plugin 接口（方向 D）

```rust
/// 排序特征：一个可计算的信号源
#[async_trait]
trait RankingFeature: Send + Sync {
    fn name(&self) -> &'static str;
    /// 特征计算的 latency budget（微秒），超时则跳过
    fn max_compute_time_us(&self) -> u64 { 5_000 }
    /// 计算一批结果的该特征值
    async fn compute(&self, ctx: &RankingContext, hits: &[SearchHit]) -> Vec<f64>;
}

/// 排序管线：注册特征 + 配置权重
struct RankingPipeline {
    features: Vec<Box<dyn RankingFeature>>,
    weights: HashMap<String, f64>,   // 从配置文件加载
    fallback_score: f64,             // 特征超时时的默认值
}

impl RankingPipeline {
    async fn rank(&self, ctx: &RankingContext, hits: Vec<SearchHit>) -> Vec<ScoredHit> {
        // 1. 并行计算所有特征（每个特征有独立的超时）
        // 2. 加权融合
        // 3. 降序排序
    }
}
```

**向后兼容策略**：当前 `merge_hits` 保持不动，新 RankingPipeline 作为可选升级。`GET /api/rooms/:id/search?ranking_v2=true` 切换。旧行为（等权融合）作为内置的 `BaselineFeature`。

### 3.3 BridgeTransport 接口（方向 C）

```rust
#[async_trait]
trait BridgeTransport: Send + Sync {
    /// 桥接一个 RoomEvent 到远端集群
    async fn bridge_event(&self, event: BridgedEvent) -> Result<(), BridgeError>;
    /// 检查远端集群存活状态
    async fn health_check(&self) -> Result<ClusterHealth, BridgeError>;
    /// 注册远端集群的回调（HTTP relay 模式需要）
    fn set_event_handler(&mut self, handler: Box<dyn Fn(BridgedEvent) + Send>);
}

struct BridgedEvent {
    source_cluster: ClusterId,
    room_id: RoomId,
    event_type: BridgedEventType, // Message | Edit | Delete | Reaction | ...
    payload: serde_json::Value,
    origin_timestamp: chrono::DateTime<Utc>,
    ttl_seconds: u32,  // 防止桥接循环
}
```

**关键设计决策**：`BridgedEvent` 携带 `ttl_seconds`（Time-To-Live）。集群 A 桥接到 B → B 的处理中如果 room 也配置了桥接回 A，则 `ttl` 递减 → 到零时丢弃。这是最简单的防循环机制（参考 IP TTL），无需跟踪消息 ID 去重。

### 3.4 信誉上下文接口（方向 E）

```rust
/// 请求处理管线的环境值
#[derive(Clone)]
struct ParticipantContext {
    // 基础身份
    auth: AuthUser,
    // 信誉数据（由中间件懒加载）
    reputation: Option<ReputationScore>,
    // 动态限流档
    rate_limits: RateLimitProfile,
    // 滥用标记
    abuse_signals: AbuseSignals,
}

struct ReputationScore {
    score: u16,           // 0-100
    tier: ReputationTier, // New | Established | Trusted | Verified
    last_updated: DateTime<Utc>,
}

enum ReputationTier {
    New,          // 注册 < 7 天 或 未验证邮箱
    Established,  // 7+ 天 + 已验证邮箱
    Trusted,      // 30+ 天 + 已验证邮箱 + SSO/企业域
    Verified,     // 人工验证 + 绑定手机
}
```

**注入方式**：作为 Axum 中间件在 `AuthUser` extractor 之后插入：

```rust
// 当前
async fn send_message(auth: AuthUser, ...) -> Result<...> { ... }

// 未来（通过 FromRequestParts 自动提取）
async fn send_message(ctx: ParticipantContext, ...) -> Result<...> {
    // ctx.reputation.tier 决定限流档
    // ctx.abuse_signals.is_suspicious_ip 触发验证码挑战
}
```

---

## 4. 技术选型

### 4.1 各方向的技术需求评估

| 方向 | 是否需要新技术栈 | 推荐技术 | 选型理由 |
|------|----------------|---------|---------|
| A: 协议抽象层 | 否 | TypeScript（已在用） | 现有 SPA 就是 TS/JS，协议抽象不引入新运行时 |
| B: 离线数据层 | 是 | `idb-keyval`（IndexedDB 封装） | 最轻量，现有 web 目录零依赖策略一致 |
| B: 离线发件箱 | 是 | Service Worker `SyncManager` | 浏览器原生支持，无需引入 workbox |
| C: NATS 联邦 | 是 | NATS Gateway（配置级） | 零代码改动，纯运维配置 |
| C: HTTP 桥接 | 是 | Axum（已有） + `reqwest`（已有） | 已有依赖，新代码仅 200 行左右 |
| D: 搜索排序 | 是 | 基础特征用 Rust 编译时注册；LTR 用 ONNX Runtime | ONNX 可以在 Rust 中推理，无需 Python 服务 |
| E: 信誉层 | 否 | Redis hash（已有） + PG（已有） | 只新增数据模式，不引入新依赖 |
| 方向二: 原生客户端 | 是 | React Native | 验证报告确认这与团队 TS 技能交集最大 |

### 4.2 关键决策：React Native vs Flutter vs 纯 WebView

| 维度 | React Native | Flutter | WebView 壳 |
|------|-------------|---------|-----------|
| 代码复用度（与 Web SPA） | 高（TypeScript 共享协议层） | 中（Dart 重写 UI） | **最高**（直接复用 web/） |
| 通话引擎 | `react-native-webrtc` 成熟 | `flutter_webrtc` 较新 | Safari WKWebView 受限 |
| 后台推送 | 成熟（FCM/APNs 原生） | 成熟 | 依赖 Service Worker + 壳层桥接 |
| 性能（直播播放） | 中（JS bridge 开销） | **高**（直接 Skia 渲染） | 低（WebView 播放 hls.js） |
| 团队适配成本 | 低（TS→RN 平迁） | **高**（全栈重学 Dart） | 低 |
| **选型建议** | **✅ 推荐** | ⚠️ 仅当直播性能为硬约束 | ❌ 只能做 MVP |

**架构层面的建议**：如果选择 React Native，**协议抽象层（方向 A）是 RN 端的前置依赖**——没有 `ProtocolClient`，RN 端需要直接写 WebSocket 逻辑，与 web 端将出现两份维护。

### 4.3 第三方依赖评估标准

为这 5 个方向引入新依赖时，统一的评估框架：

```
准入门禁（必须全部满足）：
├── ✅ 纯 Rust / 纯 JS（与现有技术栈一致）
├── ✅ MIT / Apache 2.0 / BSD 许可（排除 AGPL/SSPL）
├── ✅ 在 crates.io / npm 有 1000+ 周下载量（生态验证）
└── ✅ 最后发布在 18 个月内（活跃维护）

评分（决定是否采用的权重）：
├── 安全审计记录 30%（CVE 历史、fuzz 覆盖）
├── API 稳定性 25%（semver 追溯、breaking change 频率）
├── 性能基准 20%（与当前实现对比）  
├── 包体积影响 15%（wasm 场景下尤为重要）
└── 文档质量 10%（示例完整性、迁移指南）
```

**特例豁免**：`ONNX Runtime` 虽然体积大（~30MB），但在 LTR 推理场景下是目前唯一的 Rust 原生选择（`ort` crate）。可以**按需 feature-gate**：只在 `AERO_SEARCH_LTR_ENABLED=1` 时编译 ONNX 相关代码。

### 4.4 自建 vs 采购

| 功能 | 自建 | 采购 | 建议 |
|------|------|------|------|
| 搜索 LTR 模型 | ONNX Runtime + xgboost 训练 | Algolia / Meilisearch 的搜索即服务 | **自建**（数据不出境） |
| 验证码 | 自建 challenge（计算题/滑块） | Cloudflare Turnstile（免费增值） | **采购 Turnstile**（成本极低、用户体验好、机器人防御强） |
| 移动端推送 | 已有 `aero-push` crate | Firebase Cloud Messaging | **已有，无需买** |
| 联邦消息桥 | 自建（架构核心，不能外包） | 不可采购 | **自建** |
| 跨集群 NATS | 自运维 NATS cluster | NATS Global Network | **自运维**（数据主权要求） |

---

## 5. 实施路线图

### 5.1 优先级排序（基于验证报告修正版）

```
P0（核心基础，制约其他方向）:
├── 方向 A: 客户端协议抽象层 ← 方向一和二的共同前置
├── 方向 B-P0: IndexedDB 消息缓存 ← 离线体验的基础
├── 方向 C-P0: 身份联邦（JWT 跨集群交换）

P1（高价值、独立推进）:
├── 方向 B-P1: 离线发件箱
├── 方向 C-P1: 消息桥接（NATS Gateway）
├── 方向 D-P1: Click 信号接入排序 + 中文分词
├── 方向 D-P1: 排序特征管线（插件化架构）
├── 方向 E-P1: ParticipantContext + 信誉缓存

P2（高复杂度、依赖前置）:
├── 方向 D-P2: LTR 模型（ONNX Runtime）
├── 方向 C-P2: 删除一致性 + blob 跨集群同步
├── 方向 E-P2: 自动封禁管道 + Sybil 检测
└── 方向二: 原生移动客户端（依赖方向 A 完成 + 协议层稳定）
```

### 5.2 阶段划分

**Phase 1 — 基础（2026 Q3·8周）**

```
┌──────────────────────────────────────────────┐
│ Week 1-2: 协议抽象层（方向 A MVP）            │
│  输出: web/protocol/ 目录，ProtocolClient      │
│  可独立验证: ws.js 功能不变，ProtocolClient 包装 │
├──────────────────────────────────────────────┤
│ Week 3-4: IndexedDB 缓存（方向 B-P0）         │
│  输出: StorageAdapter + 按房间缓存最近 200 条   │
│  验证: 断网后房间历史可见                      │
├──────────────────────────────────────────────┤
│ Week 5-6: Click 信号接入排序（方向 D-P1）      │
│  输出: ClickThroughFeature 插件 + merge_hits 加权 │
│  验证: 搜索点击后的排序变化                    │
├──────────────────────────────────────────────┤
│ Week 7-8: ParticipantContext（方向 E-P1）      │
│  输出: AuthUser → ParticipantContext 中间件 + Redis 信誉缓存 │
│  验证: 限流档随信誉 tier 变化                  │
└──────────────────────────────────────────────┘
```

**Phase 2 — 体验（2026 Q4·8周）**

```
┌──────────────────────────────────────────────┐
│ Week 1-3: Service Worker + 离线发件箱         │
│  输出: sw.js + SyncManager 集成              │
│  验证: 关闭标签页发消息 → SW 在后台同步成功    │
├──────────────────────────────────────────────┤
│ Week 4-6: 身份联邦（方向 C-P0）+ NATS Gateway │
│  输出: federated_rooms migration + BridgeTransport │
│  验证: 两个集群之间 room mirroring            │
├──────────────────────────────────────────────┤
│ Week 7-8: 中文分词 + 搜索建议（方向 D-P1）    │
│  输出: jieba-rs 集成 + /api/search/suggest    │
│  验证: 中文搜索词召回率提升 40%+              │
└──────────────────────────────────────────────┘
```

**Phase 3 — 增长（2027 H1·机动）**

```
├── 排序特征管线完整化（D）
├── 自动封禁管道（E）
├── React Native 客户端立项（方向二）
├── LTR 模型训练管线（D-P2）
└── 跨集群搜索 + 合规级联删除（C-P2）
```

### 5.3 风险点和缓解策略

| 风险 | 影响方向 | 概率 | 严重度 | 缓解策略 |
|------|---------|------|--------|---------|
| Service Worker 更新策略导致用户卡在旧 JS | B | 中 | 高 | SW 使用 `CLIENT_CACHE` 策略，不缓存 `app.js`；SW 版本号 + `skipWaiting()` 组合触发更新 |
| IndexedDB 在 iOS Safari 私有模式下不可用 | B | 低 | 中 | 检测 private mode（`requestFileSystem` 技巧），降级为纯在线模式 |
| 中文分词 jieba-rs 内存超限（~200MB） | D | 中 | 中 | 评估 `pangu`（纯 Rust 轻量分词）作为替代；jieba-rs 做 feature-gated |
| NATS Gateway 网络分区导致消息黑洞 | C | 低 | 高 | 添加 `bridge_event` 的 ACK 超时 + 死信队列 + 管理员告警 |
| ONNX Runtime 编译时间过长（~10min） | D | 高（仅 CI） | 低 | 作为可选 feature（`features = ["ltr"]`），CI 只在 schedule 流水线中编译 |
| 原生客户端开发分散 Web 端资源 | 二 | 中 | 中 | 协议抽象层（方向 A）完成后 Web + Native 可以并行，减少资源争用 |
| 验证码（Turnstile）增加用户摩擦 | E | 中 | 中 | 只对低信誉（New tier）用户触发；A/B 测试验证转化率影响 |

### 5.4 不推荐的方向（基于验证报告 + 架构评估）

- **完整的 CRDT 离线同步**：虽然学术上优雅，但 CRDT 的合并语义与现有 `RoomEvent` 的编辑/删除语义存在根本性的语义冲突（并发编辑→需要 `cursor` 合并，而当前消息是纯文本替换）。建议离线模型保持**最终一致性 + 最后写入者胜出**，复杂度可控。
- **全量 OpenAPI 契约**：验证报告指出当前 OpenAPI 是示意性的。生成全量 OpenAPI（`utoipa`）的收益不匹配投入——当前 WS 帧是核心接口，REST 是辅助。建议保持手写文档，只对频繁变化的公共接口（搜索、通话信令）补充 OpenAPI。
- **Service Worker 中的 WebSocket 直连**：SW 生命周期不可控（无法维持长连接），WebSocket 必须保持在页面线程。SW 只做消息 relay + 离线同步触发。

---

## 总结：架构演进的原则

1. **协议优先，平台次之**：先抽象 `ProtocolClient`（方向 A），再考虑 PWA（方向一）和原生客户端（方向二）。协议是架构的核心骨架，UI 只是皮肤。
2. **数据先采集，再消费**：`search_click_events` 已经采集但未消费的教训不应重演。每一次新增的数据收集必须要有**明确的消费者 + 消费时间表**。
3. **信誉是横切关注点，不是独立功能**：方向 E 不应做成独立模块，而是嵌入到限流、AI 审核、搜索、通话的业务逻辑中——用 `ParticipantContext` 这样最小的接口，避免大范围重构。
4. **联邦是网络拓扑问题，不是应用层问题**：NATS Gateway/Leaf Node 可以先解决 80% 的消息同步需求，应用层只需处理边界情况（删除一致性、访客权限）。优先利用基础设施能力，而非在应用层重造桥接协议。
