现在我有了完整的信息来编写分析。以下是综合的架构评估。

---

# Aero IM 架构分析报告

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 展现出令人印象深刻的架构成熟度。**事件驱动 + 进程内扇出**模式是一个经过深思熟虑的决策，为系统的许多质量属性奠定了基础。

**优势一：清晰的 crate 分层与依赖治理。** 依赖关系严格自下而上（`aero-common` → `aero-bus`/`aero-storage` → `aero-im-core`/`aero-ai` → `aero-server`），无循环依赖。这使得单个 crate 可以在其自己的边界内独立演进，依赖链低层的 crate 保持轻量级。`aero-common` 被严格限制为叶子 crate——它没有引入 `sqlx`、`tokio` 或 `async-nats`，这对于保持编译时间和依赖图整洁至关重要。

**优势二：统一的事件回放抽象。** 房间实时性和直播实时性共享相同的核心模式——NATS subject + per-subject 单调序列号（`bus/seq.rs`）+ Hub 本地扇出。这意味着两个域复用相同的序列保证、at-least-once 交付和去重机制。durable consumer (`aero-server`) 与 ephemeral consumer (`live.stream.*`) 之间的架构分离是一个正确的设计决策，反映了房间事件（必须 exactly-once）和直播事件（可以容忍少量丢失）的不同交付保证。

**优势三：集群级状态与进程级视图的正确分离。** Redis sorted-sets 管理集群范围的状态（presence、观看者计数、通话 roster），而每个节点的 Hub 维护扇出视图。这避免了在多实例场景中由于进程内存不一致而导致的整个类别的错误。尤其明智的是，`SfuRouter` 保持在进程内（`Arc<RwLock<HashMap>>`）——SFU 路由表本质上是节点本地的；通过 Redis 共享它意味着每个 RTP 数据包都要经过额外的往返。

**优势四：全面的 AI 集成，具有优雅降级。** 系统在所有 AI 路径中都有 `HashEmbedder`（确定性、固定 1024 维输出，与 voyage-3 对齐）作为后备。这意味着沙箱/演示部署不需要 API 密钥，并且每条代码路径都保持可测试。预算系统（每个工作区每个窗口 120 个加权单位，由 AiWorker 强制执行）为多租户成本控制提供了坚实的基础，这种控制方式可以在不丢作业的情况下优雅地退避。

**优势五：媒体面深度。** str0m SFU 实现不仅仅是选择性转发——它具有时序/序列号重映射、Simulcast 分层选择（关键帧边界切换）、RTCP PLI/FIR 反馈和 H.264 关键帧检测。SRT 实现具有完整 HSv5 握手、AES-CTR 加密（RFC 3394 密钥包裹）和 ACK/NAK 可靠性。媒体栈的基线证明了在实时通信领域的高技术投资。

### 1.2 架构中的瓶颈与张力

尽管架构稳固，以下几个领域显示出张力或即将出现的瓶颈：

**领域一：通知层的渐进复杂化。** 通知路径已经演变为一个多层管道——`NotifyBatch` bus 事件 → `notification` 表（持久性收件箱）+ `activity_feed` 表（动态 feed）+ `notification_bundles` 表（聚合）+ `push_bot`（FCM/APNs）= 4 个独立的写入路径服务于一个业务概念。这种演变的代价是幂等守卫只存在于 `notification` 表（通过 NATS `seq` 去重），而 `activity_feed` 中的 `golive_bot` 写入是非幂等的（重复投递 = 重复行）。这种安排在逻辑上是一致的，但随着更多通知类型（审批、任务分配、频道公告）的加入，如果没有统一的服务层，将会出现竞态条件和部分失败的爆发点。

**领域二：主数据库上的分析查询。** `workspace_analytics`（`COUNT(*)` 全表扫描）在高流量工作区中直接对 PG 主库运行。当前在单租户/低规模下可以接受，但这些查询将开始与在线事务竞争缓冲池和工作进程。没有独立的分析副本，没有物化视图刷新，也没有将分析事件卸载到单独管道的能力。

**领域三：SFU 多轨道支持缺口。** 虽然前端已经支持 1:1 和 mesh 通话中的屏幕共享，但底层的 `SfuPeer`/`SfuForwarder` 架构是围绕单个视频轨道设计的。没有 `TrackKind` 枚举，没有 `stream_id` 复用概念，也没有用于将屏幕共享与摄像头视频相关联的 TrackId 到 SSRC 映射。在浏览器 `addTrack(screenTrack)` 触发 renegotiation 后，SFU 必须能够将多个媒体流（摄像头 + 屏幕）关联到同一个 peer 会话，而当前的转发器架构是在单轨道假设下工作的。

**领域四：事件模式演变的被动姿态。** `RoomEvent`、`StreamEvent` 和 `CallEvent` 都没有 `schema_version` 字段，并且没有变体使用 `deny_unknown_fields`。虽然这在演进过程中提供了灵活性（过时的 consumer 会忽略新字段），但在以下方面也产生了隐性成本：(a) 没有机制检测破坏性变更，(b) persisted NATS 消息是哑 JSON，没有版本标签，(c) `seq.hs` 注释中提到的 per-subject 序列保证假定 schema 兼容性，但不强制执行。

### 1.3 技术债务观察

1. **存储层中的 Token helper 命名空间冲突**——`generate_token`/`hash_token` 在 `webhook`、`scim` 和 `invitation` 之间重复。已承认（AGENTS.md §4.2 规定只从根目录 re-export webhook 版本），但这使得代码发现变得脆弱。

2. **SFU 媒体会话在测试之外未被调用**——`sfu_media_session::bind()`→`run()` 只能从 `#[cfg(test)]` 代码路径访问。`AGENTS.md` 正确地将其标识为“生产未接线”，但这是一个非平凡的风险——生产就绪路径（ICE/DTLS/SRTP 握手）的端到端回归将无法通过 CI 单元测试捕获。

3. **`golive_bot` 的非幂等写入**——如 AGENTS.md 所承认的。At-least-once 投递意味着直播开播通知可能在故障转移时重复。虽然不是立即的关键问题（用户看到两个开播通知是一种烦恼而非数据损坏），但它违反了为付费/外发/计费副作用设定的幂等约束。

4. **限流配置中的单下划线环境变量**——`AERO_RATE_LIMIT_PER_SEC`、`AERO_AUTH_RATE_LIMIT_PER_SEC` 等与双下划线 `AERO__SECTION__KEY` 约定不一致。这造成了认知负载——团队中的任何人都可能错误地尝试 `AERO__RATE_LIMIT__PER_SEC`。

---

## 2. 扩展方向

### 方向一：统一通知服务（P1 — 高业务价值，中等技术难度）

**为什么需要：** 当前系统有四个不同消耗者向三个表写入通知——`notification`（提及/回复/反应）、`activity_feed`（开播/未接来电）、`notification_bundles`（聚合）、`push_bot`（FCM/APNs 出站）。审批流、任务分配、频道公告和即将推出的日历事件都将添加更多通知类型。如果没有统一的服务抽象，每次新添加都会复制幂等守卫、读跟踪、重要性评分和推送路由逻辑。

**核心问题和技术难点：**

- 统一写入路径必须处理幂等性——NATS 序列号提供去重，但跨表原子性（Postgres 事务）与总线扇出之间存在时间差
- 通知偏好有不同的范围——频道级（静音）、工作区级（DND）、关键词级（`keyword_alerts`）、线程级（`thread_subs`）和一次性（snooze）。一个统一的 `Notifier` 服务必须在通知产生之前评估所有五个过滤器
- 聚合窗口（将 N 个回复折叠为 1 个 bundling 条目）需要一个定时器/分批策略，与现有每消息写入竞争

**提议的架构变更：**

```
当前状态：
  RoomEvent → ImService → publish_room_event → NATS
    → run_bus_listener → NotifyBatch 展开
      → notification 表写入（每目标一行）
      → activity_feed 写入（仅 golive/missed-call）
      → push_bot 消费 notification 行并推送

提议的状态：
  RoomEvent → ImService → publish_room_event → NATS
    → run_bus_listener → NotifyBatch 展开
      → NotifierService.handle(target, event)
        → 评估偏好（静音？DND？snooze？keyword 匹配？）
        → 在单个事务中写入 notification + activity_feed
        → 入队推送（仅当未静音且 importance > 阈值）
        → 根据调度偏好触发聚合
```

**对现有系统的影响：** 增量影响——`notification` 和 `activity_feed` 表保持不变。`push_bot` 从轮询 notification 表变为通过 `NotifierService` 入队。现有 bot 将继续工作，通过逐步弃用引入。

### 方向二：SFU 多轨道和录制（P1 — 高产品价值，高技术难度）

**为什么需要：** 前端已经支持 1:1 和 mesh 通话中的屏幕共享，但 SFU 架构无法将多个轨道关联到同一个 peer 会话。对于严肃的通话场景，这意味着：
- 在群组通话中启用屏幕共享会退化回全网格（每个参与者通过 N-1 个独立连接接收轨道）
- 通话录制需要 MCU/混音器——当前 SFU 是纯选择性转发，没有 RTP 混合或转码
- Simulcast 分层选择（已完成）和多轨道复用之间的交互未经测试

**核心问题和技术难点：**

- `SfuPeer` 当前维护一个单一的 SSRC 空间。添加 TrackKind（音频/视频/屏幕）和 stream_id → 重写转发器查找逻辑
- RTCP 反馈（PLI/FIR）必须针对正确的轨道——如果屏幕共享视频流丢失关键帧，触发全视频 FIR 是不正确的
- 录制需要 RTP 混合或转码——这要么意味着集成 FFmpeg（巨大的二进制大小，许可证复杂性），要么意味着基于 str0m 的轻量级 Mixer（尚未实现，复杂度高）
- 通过 call-bridge 的跨节点轨道关联——`bridge_frame` 必须携带 TrackId 元数据以便远端节点可以路由到正确的订阅者

**提议的架构变更：**

```
当前（单轨道 SFU）：
  SfuPeer { ssrc: u32, video: RtpPacket 流 }
  SfuForwarder { subscribers: Vec<Consumer> }
  查找：ssrc → SfuPeer

提议（多轨道 SFU）：
  SfuPeer { id: PeerId, tracks: HashMap<TrackId, SfuTrack> }
  SfuTrack { kind: TrackKind, ssrc: u32, rtx_ssrc: Option<u32>,
             stream_id: String, last_keyframe_at: Instant }
  SfuForwarder { subscribers: Vec<ConsumerTrackSelector> }
  查找：stream_id + kind → SfuTrack
  录制：SfuRecorder { tracks: HashMap<TrackId, RtpPacket 缓冲区> }
        通过挂钟 + rtp 时间戳进行混合
```

**对现有系统的影响：** 重大但局部化。`SfuPeer` 和 `SfuForwarder` 位于 `aero-live-webrtc` crate 中，该 crate 与其他组件（NATS、PG）没有依赖关系。变更将影响 `SfuRouter`（进程内 `HashMap`）和 `SfuMediaSession`（RTP 事件循环）。call-bridge（`CallEgress`/`ensure_egress`）需要更新 `bridge_frame` 以包含轨道路由元数据。关键是要为 `TrackKind` 添加单元测试（音频与视频与屏幕），并验证 RTCP PLI/FIR 路由。

### 方向三：事件 Schema 注册表和版本化（P2 — 中等业务价值，中等技术难度）

**为什么需要：** 目前有三种事件类型（`RoomEvent`、`StreamEvent`、`CallEvent`），所有变体都没有版本标签。随着系统的发展，模式会演变得超出临时兼容性所能处理的范围。具体风险：(a) 恢复旧 NATS 消息的持久 consumer 可能会在反序列化新变体时失败，(b) 没有结构化方式来表示弃用或迁移，(c) Webhook 订阅者无法协商他们理解的事件形状。

**核心问题和技术难点：**

- 引入 `schema_version` 字段需要在写入端和读取端进行协调——现有消费者必须在升级前处理新版本
- 版本协商需要每个事件的开放/封闭变体策略——添加新字段（非破坏性）对比重命名/删除字段（破坏性）
- serde 的 `deny_unknown_fields` 不能通过 `#[serde(deny_unknown_fields)]` 显式应用（代码注释提到避免它）。因此，过时的消费者静默丢弃未知字段——这在过渡期间是可取的，但在长期内会掩盖兼容性问题
- NATS JetStream 主体是哑字节——没有内置的 schema 注册表。必须自建或集成 Confluent SR（不匹配，因为不是 Avro/Protobuf）

**提议的架构变更：**

```
RoomEvent 上的 #[derive(Serialize, Deserialize)] 扩展：
  schema_version: u32  // serde(default = "1") 用于向后兼容

事件注册表（可以是嵌入式 Rust 模块或轻量级 SQLite）：
  EventSchema { name: &'static str, current_version: u32,
                migrations: Vec<EventMigration> }

读取路径：
  1. 反序列化 { schema_version, kind, data... }
  2. 如果 schema_version < current_version：应用从 v 到 current_version 的迁移
  3. 反序列化为当前的 RoomEvent 变体

发布者强制要求：
  schema_version = CURRENT_EVENT_SCHEMA  // 编译时常量
```

**对现有系统的影响：** 低，如果逐步推出。`schema_version` 的 `serde(default = "1")` 意味着所有现有消息都被视为 v1。consumer 读取 v1 消息就像现在一样。新的 consumer 通过版本检查并应用迁移。这可以在几周内与现有生产流量同时推出。

### 方向四：独立分析管道（P2 — 中等业务价值，高技术难度）

**为什么需要：** `workspace_analytics` 端点直接对 PG 主库运行 `COUNT(*)` 查询。在规模上（单个工作区有数百万条消息），这些查询会：(a) 饿死 OLTP 工作负载的共享缓冲区，(b) 触发 autovacuum 风暴，(c) 在主库上产生读 IO。此外，当前的分析仅限于消息数量——没有行为事件（文件下载、通话时长、直播观看会话持续时间、搜索查询）。

**核心问题和技术难点：**

- 分析事件收集需要一个新的 `AnalyticsEvent` 枚举和所有关键路径的仪器化——每条消息写入、每次通话结束、每次直播观看者连接/断开
- 分析存储应该是列式（ClickHouse）或时间序列（TimescaleDB）以获得正确的查询性能。这增加了新的基础设施依赖
- 在 PG 主库上，`INSERT INTO analytics_events ...` 与事务性写入竞争。解决方案：通过 NATS 将分析事件卸载到专用消费者，该消费者批量写入 ClickHouse
- ClickHouse 集成增加了部署复杂度——现在有 5 个基础设施依赖项（PG、Redis、NATS、MinIO、ClickHouse）

**提议的架构变更：**

```
当前状态：
  HTTP GET /api/workspace/:id/analytics
    → WorkspaceAnalyticsRepo::stats(ws_id)
      → SELECT COUNT(*) FROM messages WHERE room_id IN (...)

提议状态：
  业务层：
    → ImService::publish_room_event(..., 分析上下文)
    → NATS subject "analytics.room.{id}"
    → AnalyticsConsumer（新 crate 或 server 模块）
      → 批量写入 ClickHouse（每 5s 或 1000 条记录）

  读取路径：
    HTTP GET /api/workspace/:id/analytics
      → AnalyticsRepo::query(ws_id, 时间范围)
        → SELECT ... FROM analytics_mv（ClickHouse 物化视图）
```

**对现有系统的影响：** 中等。分析收集可以逐步添加——从消息量开始（已经在 `MESSAGES_SENT_TOTAL` 计数器中被追踪），然后添加通话和直播分析。在 ClickHouse 可用之前，读取路径可以回退到 PG。影响最大的早期举措是将 `workspace_analytics` 迁移到物化视图，即使在同一条 PG 实例上也能改善响应时间。

### 方向五：跨节点媒体面和 SFU 生产接线（P1 — 高运维价值，中等技术难度）

**为什么需要：** 当前的 SFU 媒体会话（`SfuMediaSession::bind()→run()`）无法从生产代码访问——它仅在 `#[cfg(test)]` 中被实例化。这意味着 ICE/DTLS/SRTP 握手路径、RTP 转发和跨节点 call-bridge 在 CI 中保持未经测试。对于真正的多节点部署，`call_bridge_supervisor::ensure_bridges`/`ensure_egress` 必须被实际接线。缺少的“最后一步”是生产就绪性的最大单一风险。

**核心问题和技术难点：**

- `SfuMediaSession` 是异步的并且有状态——它拥有套接字、解析 RTP 数据包并将它们馈送给 `SfuForwarder`。生产接线需要正确的启动/关闭生命周期，并与通话生命周期（`CallOrchestrator` 开始/结束事件）协调
- `ensure_egress` 是跨节点 UDP 中继——必须小心不要通过写入同一节点的 `SfuForwarder` 已经发送的数据来创建 RTP 回环
- 当前代码结构为测试设置了 `bind()`→`run()`——生产路径需要 `start(peer, cancel)` 与通话编排者的 `CallSession` 清理协调
- 没有多节点测试环境——验证需要至少两个 server 实例，加上一个通过 ICE/DTLS/SRTP 连接的浏览器

**提议的架构变更：**

```
boot/background.rs：
  tokio::spawn(call_bridge_supervisor::run(call_state.clone(), ...))
  // 监听通话生命周期事件 → 创建/销毁 SfuMediaSession

通话开始（CallOrchestrator::start_call）：
  → 解析 SFU 端点（本节点或远程）
  → 如果是本地：SfuRouter::register_session(peer_id, session_handle)
  → 如果是跨节点：CallBridgeSupervisor::ensure_bridge(call_id, remote_node_url)

通话结束：
  → SfuRouter::deregister_session(peer_id)
  → CallBridgeSupervisor::teardown(call_id)
```

**对现有系统的影响：** 低到中等。现有代码结构不需要重写——需要的是适当的接线。主要风险是确保 `SfuMediaSession` 的生命周期管理正确（不要泄漏套接字或悬空桥）。这应该与方向 B（多轨道 SFU）协调，因为添加轨道关联会增加接线复杂性。

---

## 3. 接口设计建议

### 3.1 NotifierService 接口原则

```rust
// 原则：命令查询分离（CQRS）——通知产生是与通知消费分离的

pub trait Notifier: Send + Sync {
    /// 评估偏好 → 在单个事务中写入 notification + activity_feed → 可选入队推送
    async fn notify(&self, target: NotifyTarget, event: &RoomEvent)
        -> Result<NotifyOutcome>;

    /// 标记为已读，不回滚
    async fn mark_read(&self, participant: ParticipantId, id: NotificationId);

    /// 批量标记，带游标推进
    async fn mark_read_batch(&self, participant: ParticipantId, ids: &[NotificationId]);
}

pub enum NotifyOutcome {
    Delivered,       // 写入 + 如果是高优先级则推送
    Suppressed,      // 静音/DND/snooze 已激活，未写入
    Aggregated,      // 合并到现有 bundle
}
```

关键设计决策：将 `Notifier::notify` 设为幂等（由 NATS 序列去重键保护），并且是同步的，以便调用者可以假定在返回 `Ok(Delivered)` 时写入已提交。

### 3.2 SFU 多轨道扩展点

```rust
// 原则：以不会破坏现有单轨道路径的方式添加 TrackKind

pub enum TrackKind {
    Audio,
    Video,       // 相机
    Screen,      // getDisplayMedia
}

pub struct SfuTrack {
    pub track_id: TrackId,
    pub kind: TrackKind,
    pub ssrc: u32,
    pub rtx_ssrc: Option<u32>,
    pub stream_id: String,     // 浏览器 MSID
    pub mid: String,           // SDP 媒体行 ID
    pub last_keyframe_at: Option<Instant>,
}
```

现有 `SfuPeer` 结构体应获得 `tracks: HashMap<TrackId, SfuTrack>`，同时保留旧的 `ssrc`/`video` 字段以便过渡期内兼容。一旦所有调用者升级，再移除旧字段。

### 3.3 事件 Schema 版本的权衡

**方案 A：编译时常量版本（推荐）**

```rust
#[derive(Serialize, Deserialize)]
pub struct RoomEvent {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,   // = 1
    pub kind: String,
    // ... 现有字段 ...
}
```

优点：零运行时开销，无需额外的协调服务，后退兼容性通过 serde 的 `deny_unknown_fields` 缺失得以保证。缺点：没有运行时代理能够验证传入的消息是否兼容。

**方案 B：Schema 注册表服务**

优点：跨所有环境的显式兼容性检查，潜在的 Protobuf/Avro 互换性。缺点：需要 ZooKeeper/etcd 类型的共识——基础设施膨胀，对 Aero 的尾部延迟要求来说过于重量级。

**建议：方案 A，加上 `scripts/compatibility-check.sh` 在新变体被添加时检测破坏性变更的编译时检查。**

### 3.4 分析管道抽象

```rust
// 原则：分析事件是 fire-and-forget，不对业务操作关键

pub trait AnalyticsBackend: Send + Sync {
    /// 记录一个行为事件。不返回错误——fire-and-forget。
    fn record(&self, event: AnalyticsEvent);

    /// 查询已存储的分析数据。
    async fn query(&self, workspace: WorkspaceId, range: TimeRange,
                   metrics: &[Metric]) -> Result<AnalyticsResult>;
}

pub struct AnalyticsEvent {
    pub timestamp: OffsetDateTime,
    pub kind: AnalyticsEventKind,
    pub workspace_id: WorkspaceId,
    pub actor_id: ParticipantId,
    pub metadata: HashMap<String, String>,
}

// ClickHouse 实现：缓冲写入，每 N 秒刷新一次
// Postgres 回退实现：INSERT + SELECT COUNT(*)，用于演示/单节点
```

---

## 4. 技术选型

### 4.1 需要引入的技术栈

| 组件 | 推荐 | 替代方案 | 评估 |
|------|------|---------|------|
| **分析存储** | ClickHouse | TimescaleDB (PG 扩展) | ClickHouse 在列式存储 + 物化视图 + 插入吞吐量方面胜出。TimescaleDB 简化部署（重用同一个 PG 实例），但分析查询仍然与 OLTP 竞争 IO |
| **通话录制/混音** | 基于 FFmpeg 的子进程 | 自建的纯 Rust 混音器 | FFmpeg 增加了 30+ MB 二进制大小和 GPL 许可证复杂性。基于 str0m 的纯 Rust 混音器是理想选择，但需要大量工程投入。短期：将 SFU RTP 流导出到文件，然后通过 FFmpeg 子进程后期处理 |
| **Schema 注册表** | 编译时检查 + 嵌入式测试 | Confluent SR + Protobuf | 嵌入式方法在没有新基础设施开销的情况下捕获 90% 的兼容性问题。Protobuf 比 JSON 严格，但会破坏事件演变的 serde 灵活模式 |

### 4.2 第三方依赖评估标准

```
标准（按重要性排序）：
1. 许可证：必须与 Rust 2021 / MSRV 1.80 兼容。Apache 2.0 / MIT 首选。
   GPL/AGPL 需要法律审查（FFmpeg 的 GPL 是边界情况）。
2. 活跃维护：过去 12 个月内至少有一次提交。
3. 安全审计：对于加密/网络代码，首选出过安全公告和有 CVE 响应历史的库。
4. 测试覆盖：依赖的测试不应完全集成——必须有单元测试验证 Rust 类型边界。
5. 二进制包大小：像 FFmpeg 这样的原生依赖不应通过链接进入 `aero-server` 二进制文件
   ——通过子进程生成单独处理。
```

### 4.3 自建 vs 采购决策矩阵

| 能力 | 自建 | 采购/集成 | 决策 |
|------|------|-----------|------|
| **通话录制混音** | 纯 Rust + str0m | FFmpeg 子进程 | **自建**混音器。FFmpeg 的 GPL + 大小开销是不值得的。轻量级 Mixer 可以在 ~500 行 Rust 代码中实现（PCM 音频混合 + H.264 切换），与 SFU 体系结构匹配 |
| **分析/BI** | ClickHouse + 自建查询 | 嵌入 Metabase / Grafana | **自建**查询端点保留细粒度访问控制（工作区范围）。Metabase 对于嵌入式多租户来说是一个认证/授权噩梦 |
| **Schema 注册表** | 编译时宏 + 测试 | Confluent SR | **自建**。Confluent SR 是针对 Protobuf/Avro 生态系统设计的，不适合 serde JSON 事件模型 |
| **Push 通知** | 现有 `FakeGateway` + FCM/APNs | Firebase Cloud Messaging SDK | **已在使用**。FCM 网关是第三方。不要用原生 SDK 替换——保持服务端 FCM 集成 |

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 理由 |
|--------|------|------|
| **P0** | 方向五：跨节点媒体面和 SFU 生产接线 | 真正的业务阻塞——没有生产 SFU，通话在多于一个节点时会退化。接线 `SfuMediaSession::run()` 是解锁通话录制、多轨道和跨节点桥的前提条件 |
| **P0** | 方向二：SFU 多轨道支持 | 屏幕共享（已经在前端实现）在 SFU 架构中损坏。这是产品差异化因素——Slack 有屏幕共享，Teams 有屏幕共享——而 Aero 的 SFU 无法路由它 |
| **P1** | 方向一：统一通知服务 | 高业务价值——每个新的通知类型（审批、任务、公告）都会增加重复逻辑的成本。现在进行重构可以防止未来出现更昂贵的修复 |
| **P2** | 方向三：事件 Schema 版本化 | 低风险，高长期价值。早期开始可以在 Schema 漂移变得不可逆之前建立模式 |
| **P2** | 方向四：分析管道 | 对业务增长关键（客户想要数据），但不是实时的。物化视图迁移作为第一步可以在零基础设施变更的情况下显著改善 |

### 5.2 阶段划分

**阶段 1（4-6 周）— 媒体面稳定化：**

- 将 `SfuMediaSession::bind()→run()` 接线为从 `CallOrchestrator` 生命周期钩子调用
- 添加多节点集成测试（两个 server 实例，一个浏览器 ICE 握手）
- 在 `SfuPeer` 中添加 `tracks: HashMap<TrackId, SfuTrack>` 以及迁移期兼容性
- 更新 `SfuForwarder` 以按 TrackKind 路由：视频 → 摄像头订阅者，屏幕 → 屏幕订阅者
- 更新 `bridge_frame` 以携带轨道路由元数据
- 验证现有的 `toggleScreenShare` / `gcallToggleScreenShare` 前端代码通过 SFU 工作

**阶段 2（3-4 周）— 通知重构：**

- 提取 `NotifierService` trait + `PostgresNotifier` 实现
- 将所有通知写入迁移到统一入口（`notification`、`activity_feed`、`push_bot` 入队）
- 为 `golive_bot` 添加幂等键（`ON CONFLICT (participant_id, seq) DO NOTHING`）
- 弃用旧的直接写入模式

**阶段 3（4-6 周）— 可观测性和模式治理：**

- 向 `RoomEvent`、`StreamEvent`、`CallEvent` 添加 `schema_version` 字段（`#[serde(default = "1")]`）
- 实施编译时兼容性检查（检测没有 `deny_unknown_fields` 的新变体）
- 将 `workspace_analytics` 迁移到物化视图
- 添加带 ClickHouse 集成的 `AnalyticsEvent` 枚举（可选——如果 PG 性能足够好则跳过）

### 5.3 风险和缓解策略

| 风险 | 可能性 | 影响 | 缓解 |
|------|--------|------|------|
| SFU 多轨道重构破坏了现有 1:1 通话 | 中 | 高 | 维护 `SfuPeer` 向后兼容性（保留旧字段，仅在后面添加 `tracks`）。验证 CI 中的回归测试套件（841 个测试） |
| 通知统一引入了回归：静音/免打扰配置被忽略 | 中 | 中 | 为每个通知类型添加集成测试（提及通过，静音频道的提及不起作用）。针对 postgres-notification-store 进行隔离测试 |
| ClickHouse 部署增加了运维复杂性 | 低 | 中 | 首先物化视图迁移——无基础设施变更。使 ClickHouse 集成可选（编译特性标志） |
| Schema 版本化使得开发者抗拒添加新字段 | 中 | 低 | 最小化样板：`add_field_v2!` 宏或仅一个 `schema_version` 常量 + `From<OldEvent>` impl |
| 跨节点接线引入了 RTP 回环 | 低 | 高 | 在 `ensure_egress` 中通过源节点身份进行明确的环路检测。如果 `source == local_node`，则不转发。单元测试验证无回环 |

### 5.4 依赖关系图

```
阶段 1（媒体面稳定化）
  └── 阶段 2（通知重构）
        └── 阶段 3（Schema 版本化）
              └── 分析（独立，可以与任何阶段并行）

阶段 1 和阶段 2 可以并行进行（不同 crate，无共享代码扰动）。
阶段 3 仅依赖于阶段 1 和阶段 2 的完成，用于全面生产验证，
但大部分实现可以独立完成。
```

---

## 结论

Aero IM 的架构展示了深思熟虑的设计决策和高技术执行标准——特别是在事件驱动框架、AI 集成成本和媒体面深度方面。五个提议的扩展方向中，两个包含需要纠正的事实错误（屏幕共享状态和通知统一），但潜在的技术方向是合理的，并且在架构上与本系统兼容。

**首要行动项目：**

1. 在媒体面方向中承认前端屏幕共享已经存在。将重点转移到 SFU 多轨道路由，与前端实现匹配
2. 承认 `activity_feed` 中的 `read_at` 和统一通知模型——相应调整提议范围
3. 优先处理 SFU 生产接线（`SfuMediaSession` 在 `cfg(test)` 之外）——这是媒体面正确性的最重要单一瓶颈
4. 开始编译时事件模式兼容性检查——预防性治理，实施成本低
