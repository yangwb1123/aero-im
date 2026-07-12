好的，我已经仔细阅读了项目上下文（AGENTS.md）以及你提供的验证分析文档。下面我来做一次深入的架构分析。

---

# Aero IM 架构分析报告

## 1. 架构评估

### 1.1 当前架构的优势

**分层明确，依赖单向。** crate 地图从 `aero-common`（叶子）→ `aero-bus`/`aero-storage`/`aero-auth` 等基础层 → `aero-im-core`/`aero-live-*` 业务层 → `aero-server` 组合层，无成环依赖。这使各 crate 可独立编译、测试、演化，是 Rust monorepo 的较优实践。

**事件骨架构图合理。** "业务层产事件 → NATS subject 跨实例投递 → 每进程 Hub 本地扇出到 WebSocket" 的三段式设计：
- 业务层与扇出解耦（业务代码只需 `publish_room_event`，不问谁消费）
- NATS JetStream 提供 at-least-once 持久化和跨实例水平扩
- Hub 内 `bounded mpsc` + `try_send` 避免扇出背压阻塞发送者

**Bot/Worker/Timer 治理清晰。** AGENTS.md §2 以表格 + 关键不变量形式记录了每个常驻智能体，包括触发条件、行为约束、幂等守卫、fail-open/fail-closed 策略。这种"可审计的运行时实体清单"是系统运维的基石。

**Redis 集群状态 + 进程内 SFU 路由分离得当。** 跨实例一致性状态（presence、观看数、通话 roster）走 Redis sorted-set，而 SFU peer 路由（`SfuRouter`）是进程内 `Arc<RwLock<HashMap>>`，不混为一谈。这避免了 SFU 转发决策引入跨节点共识延迟。

### 1.2 关键设计决策评估

| 决策 | 评估 | 风险/债务 |
|------|------|-----------|
| NATS 作为唯一跨实例事实源 | ✅ 合理。JetStream 持久化 + at-least-once 语义成熟 | 单点依赖——NATS 故障全集群实时通信中断。需对 NATS 集群本身做高可用 |
| dispatch_notifications DB 查询展开成员 | ⚠️ 早期合理，但可能成为规模瓶颈 | 每消息 N 次 DB 往返（成员列表 + 通知偏好 + 静音门控）。在 10 万成员频道发消息时可能是灾难 |
| 全局游标 `?since=` 重放 | ⚠️ 有架构债 | 跨房间重放覆盖缺口 + 重放无用消息（客户端订阅了 50 个房间但只关心 3 个，却收到全部消息） |
| @everyone/@channel 无大小限制 | ❌ 有架构债 + 安全风险 | 10 万成员频道的 @everyone 可触发推送风暴 + DB 成员展开风暴 |
| Hub 扇出无界 mpsc | ⚠️ 我的阅读可能偏差——AGENTS.md 写 "bounded mpsc" 但分析文档写 "无界 mpsc + try_send" | 如确实无界，慢消费者会导致内存无限增长。请确认代码 |
| 迁移编译期嵌入 | ✅ 工业级实践，保证迁移版本与代码绑定 | 运作成本略高（改迁移需 rebuild），但值 |
| AI 无 key 退化 | ✅ Fail-open 设计正确 | 退化路径（HashEmbedder + 启发式 completion）需保证接口语义一致（沙箱路由也 200），已做到 |

### 1.3 架构债务识别

以下按严重程度排列：

**P0 债务：`@everyone` / `@channel` DoS 放大器**

`dispatch_notifications` 当前对"提及"场景的接收者展开无任何速率限制或大小上限。在 10 万成员工作区，一个 `@everyone` 就会触发：
- 1 次成员列表全量查询（10 万行）
- 10 万次通知偏好查询（逐个判断是否静音/免打扰）
- 非静音成员：每成员插入通知行 + WebSocket 帧 + 可能的推送通知

如果恶意用户以 1rps 发 `@everyone`，系统在消息级有限流（`check_ws_rate_room`），但**每条消息的内部扇出仍然会重做全部 10 万次操作**。这是 CPU + DB + NATS + 推送四重放大器。

**P1 债务：全局游标重放协议**

当前 `?since=` 方案：
- 跨房间重放覆盖缺口（时序窗问题，已验证）
- 重放大量客户端不关心的消息（浪费带宽、客户端内存、解析时间）
- 客户端重连时无法精确恢复未收消息

已在 `delivery_cursor.rs` 实现了 per-room 游标（表已迁移 0153），但 `web/ws.js` 从未使用——这是一个"半截子工程"债务。

**P1 债务：dispatch_notifications 的 DB 往返规模**

虽然是方向一的分析主题，但值得指出：`NotifyBatch` 压缩了 NATS 扇出是好设计，但**没有解决接收者展开后的 DB 查询规模**。每条消息在 `dispatch_notifications` 中循环 `for member_id in &recipients` 逐个查 `notif_prefs`，这是一个 O(N) DB 查询模式。

**P2 债务：Redis fail-open 的 @here 退路风险**

AGENTS.md §4.3 规定 Redis 故障 fail-open（服务继续）。对于 `@here` 的退路（Redis 存在线状态，故障时回退全成员），在 10 万成员频道中是真实的风险。需要至少做在线名册的本地缓存副本做降级，而不是直接回退全量。

**P2 债务：Push 风暴**

`push_bot` 当前无限流——每注册设备一条推送。如果一个工作区有 5 万活跃用户 × 平均 2 设备 = 10 万推送。这不仅是 FCM/APNs 配额问题，也是 NATS 消息量的放大器（`push_bot` 收到一条 `Notify` → 展开 N 条推送请求）。

---

## 2. 扩展方向

### 方向一：提及放大器拦截层（Mention Amplifier Guard）

**为什么需要？**

`@everyone` 和 `@channel` 当前的 DoS 风险是产品级问题。一个流氓内部用户可在 30 秒内让 10 万成员每人收到 30+ 条推送通知——这会触发 FCM/APNs 速率限制（甚至封禁）、DB 负载飙升、WebSocket 帧积压。

这不仅是技术问题，也是**产品信誉问题**：一个 `@everyone` 无法取消，成员体验极差。

**核心挑战**

1. **展开前拦截**：需在接收者展开之前（`dispatch_notifications` 内）拦截，而不是在消息发布之后（那时已经产生了 DB 查询成本）
2. **配置纬度**：速率限制应该是 per-room（大频道更严格）、per-role（管理员放宽）、per-time-window（静默期禁止）的组合
3. **感知层通知用户**：当 `@everyone` 被限，发送者需收到即时反馈（而非"消息发了但实际没通知任何人"）

**架构变更**

```
现有：
  Message → publish_room_event → dispatch_notifications → 展开 members → 插通知

变更后：
  Message → publish_room_event → MentionAmplifierGuard ─┬─ pass → dispatch_notifications (正常)
                                                          └─ block → 回写提醒消息 + 扇出 warning 帧
```

`MentionAmplifierGuard` 作为一个**中间件层**，在 `ImService` 中（或 bot 消费前）拦截，而非独立 bot——因为它是安全守卫，不是异步作业。

- 配置：工作区级别 `max_mention_recipients` + per-room `mention_cooldown_secs`
- 实现：Redis 计数（原子递增 + TTL），而不是进程内存（跨实例一致）
- 回退：Redis 故障时**fail-closed**（拒绝 @everyone 展开）而不是 fail-open——安全守卫应保守

**对现有系统影响**

- `ImService`（`im-core/service/`）新增 `check_mention_amplification` 方法
- `dispatch_notifications` 需将展开后的提及大小传回调用方
- 新增 `RoomEvent` variant（如 `MentionBlocked`）或复用 `System` 事件
- WebSocket 端需新增 `msg:mention_blocked` 帧处理

### 方向二：逐房间交付游标协议扩展（Per-Room Delivery Cursor）

**为什么需要？**

如方向三分析所示，当前 `?since=` 全局游标有跨房间覆盖缺口。`delivery_cursor` 表已存在，但前端从未使用。这是一个"已完成一半的架构改进"。

业务价值：
- 客户端重连更精确（只丢失未收消息）
- 带宽节省（不重放不需恢复的房间的消息）
- 大规模工作区的首次连接体验（新设备加入时只拉历史，不必一次性 replay 全部房间）

**核心挑战**

1. **协议兼容性**：旧客户端（只发 `?since=`）必须继续工作。新客户端发 `?cursors={room_id: seq, ...}` 服务端需双通道支持
2. **游标持久化与 GC**：`delivery_cursor` 表可能膨胀——每个成员在每个房间都有一个行。10 万成员 × 100 房间 = 1000 万行
3. **seq vs timestamp**：当前 `since` 是基于 `created_at` 时间戳的 MessageId。`delivery_cursor` 应使用 `last_seq`（每个房间单调递增的 seq，见 `bus/seq.rs`），因为 seq 更可靠（无时钟偏差）

**架构变更**

```
协议层：ws.js 新增连接参数 ?cursors={json}
服务层：WsConnection::handle_connect 增加解码分支
        如是 cursors 模式 → 跳过全局 replay → 调用 delivery_cursor::cursors_for 逐房间补发
        如果是 since 模式 → 按现有逻辑处理（向后兼容）
存储层：delivery_cursor repo 需新增 batch_advance（原子性推进多个游标）
```

**预期变更**

- `ws/mod.rs` 的 `handle_ws_upgrade`：解析 `cursors` 参数
- `ws/ws_impl/bus.rs`：新增 `replay_from_cursors` 函数
- `storage/src/delivery_cursor.rs`：新增 `batch_advance` 和 `gc_stale_cursors`
- 新增迁移：给 `delivery_cursor` 加适当的索引（复合索引 `(participant_id, room_id)`）
- 新增定时器：定期清理不再活跃的游标（参与者在某房间无新消息超过 30 天则清理）

**风险与缓解**

| 风险 | 缓解 |
|------|------|
| 游标表膨胀 | 定期 GC + 分区（按房间分表）或 TTL 过期 |
| 旧客户端兼容 | 服务端双通道支持 `since` 和 `cursors`，`cursors` 优先 |
| 游标推进竞争 | 使用 `bus/seq.rs` 的 `next_seq`（原子递增），非乐观看 |

### 方向三：通知扇出从"逐行查询"转向"批量展开"（Batch Notification Fan-out）

**为什么需要？**

当前 `dispatch_notifications` 的核心瓶颈是 per-recipient 的 DB 查询。虽然对 1000 成员房间尚可，但在 10 万成员规模下，单条消息的展开成本 ≈ 10 万次 `notif_prefs` 查询 + 10 万次通知插入 + 10 万次 WS 扇出尝试。

**核心挑战**

1. **通知偏好存储**：当前 `notif_prefs` 是逐行存储。批量展开意味着需要一次查询拿到全房间的通知偏好
2. **静音门控**：DND 时段、snooze 状态、关键词静音的组合过滤
3. **推送过滤**：在展开阶段就判断某成员是否需要推送（在线则不推），而不是在 `push_bot` 再过滤

**架构变更**

方案 A（推荐，低侵入）——**Redis 缓存通知偏好**：

```
原路径：Message → 逐个查询 PG notif_prefs × N 次
新路径：Message → Redis HGETALL room:{id}:notif_prefs（1 次 O(1)） → 内存过滤 → 批量插入
```

- Redis 中存 `room:{rid}:notif_prefs` 为 hash，key=member_id, value=json(prefs)
- `notif_prefs` CRUD 路径写入时同时更新 Redis
- Redis 故障时回退 PG（走慢路径但功能不变）
- 需要：`aero-storage` 新增 `NotifPrefCache` 层，`put`/`get_room_batch` 方法

方案 B（高收益，高侵入）——**重写通知引擎为基于房间的批量展开**：

- 将 `dispatch_notifications` 的循环从 `for member in recipients` 改为 `for room in &rooms { batch_fetch_prefs(room); batch_insert_notifications(room); }`
- 需要先做方向一的 `MentionAmplifierGuard`，否则批量展开时无防护

**建议**：先方案 A（快速改善），再视规模决定是否方案 B。

### 方向四：推送风暴控制（Push Storm Gate）

**为什么需要？**

`push_bot` 当前无流控——一条 `Notify` 消息在 5000 成员房间中可触发 10000 FCM 请求。FCM/APNs 有明确的速率限制（FCM 默认约 600 次/分钟/设备令牌组，但每个项目有总速率上限）。频繁超限可能导致推送通道被封。

**核心挑战**

1. **多级限流**：需要 per-app（总量）、per-room（单房间风暴）、per-device（单个设备速率）
2. **推送去重**：同一成员在多个设备上收到同一条消息是目前的设计——是否合理？是否应设备维度去重？
3. **优先级**：DM 消息推送达 > @mention > 频道普通消息。风暴控制应区分优先级，高优先级消息跳过限流
4. **合并推送**：同设备 5 秒内多条通知应合并为一条"3 条新消息"摘要

**架构变更**

```
push_bot 增加：
  PushThrottleLayer ── per-app：Redis 滑动窗口计数
                      ├─ per-room：Redis ZCOUNT over 60s window
                      └─ per-device：本地 token bucket（低延迟，允许小 burst）

  推送合并器：
    Redis 缓存设备最后推送时间 → 如 < 5s → 不推，累加待合并计数
    定时器每秒 flush 合并推送
```

**不建议自建 FCM/APNs 网关**。`aero-push` 当前是薄网关层，直接调用 Firebase Admin SDK / APNs HTTP/2 是正确选择。需要改善的是**调用前端的限流和合并逻辑**，而不是替换推送后端。

### 方向五：流媒体直播的 CAP 分析与降级策略（Live Stream CAP Analysis）

**为什么需要？**

当前直播链路（RTMP/WHIP/SRT → HLS）对推流器稳定性有隐式假设。真实场景中推流中断、网络抖动、编码器异常是常态。系统需在"一致性 vs 可用性"之间做选择。

**核心挑战**

1. **推流中断检测**：当前 RTMP 连接断开后，如何判定"推流结束" vs "推流抖动"？需要超时 + 重连窗口
2. **HLS 切片延迟**：在断流重连后，HLS writer 应继续切片还是新开 session？当前 `LiveStreamRecord` 的生命周期管理
3. **跨节点转发的可用性**：`call_bridge_supervisor` 当前是最薄弱环节——已建但未接线。在真两节点部署时，一个节点的 SFU 故障是否应触发另一节点接管？

**架构变更**

建议分层降级策略（从已有基础设施出发，非重造）：

1. **推流健康探针**：`aero-live-core` 新增 `StreamHealthMonitor`——监听 RTMP 断开事件，启动 `grace_period` 超时，超时内重连则继续切片，超时后触发 `Status{Idle}` 事件
2. **HLS 连续性**：`HlsWriter` 在断流后保持切片序列号连续（断流段输出空 ts + `#EXT-X-DISCONTINUITY`），重连后继续 append
3. **SFU 热备（P3**）：`SfuRouter` 当前是进程内 `HashMap`。如要跨节点高可用，需要 SFU 状态同步（ssrc→node 映射），但投入产出比低。建议保持"每个节点独立 SFU，"推流器通过 WHIP 重连切换到健康节点"

---

## 3. 接口设计建议

### 3.1 ImService 接口设计原则

当前 `ImService` 是一个大外观（facade），聚合了消息、通知、通话、审核等功能。建议保持这种模式，但要**明确分界线**：

```
ImService ── 业务编排层（或chestrator）
  ├── MessageOps (send/edit/delete/react)
  ├── NotificationOps (dispatch/subscriptions)
  ├── CallOps (orchestrate/roster)
  └── ModerationOps (keyword/ai)
```

**不一定要拆成多个 trait 或 struct**——Rust 的模块系统足以做逻辑分区。关键是模块内方法签名的一致性：

- 所有 "用户对房间的操作" 签名一致：`fn action(&self, auth: AuthUser, room: RoomId, payload: Payload) -> Result<Event>`
- 所有 "系统触发的操作" 签名一致：`fn action_by_system(&self, payload: Payload) -> Result<Event>`
- 避免在 `ImService` 中添加纯查询方法（查询应走 `XRepo` 直接调用）

当前的 `publish_room_event` 接口是好的，它封装了 NATS 发布 + seq mint + 事件格式组合的复杂性。调用者不应关心 NATS。

### 3.2 需要引入的抽象层

**建议新增：DispatchPolicy 层**

当前 `dispatch_notifications` 直接在核心逻辑中做成员展开 + 通知偏好过滤 + 插入。建议抽象为：

```rust
trait NotificationDispatcher {
    /// 产出一组待推送的通知，不负责实际发送
    fn resolve_recipients(&self, event: &RoomEvent, room: &Room) -> Vec<Recipient>;
    /// 逐批次插入通知并触发扇出
    fn dispatch(&self, recipients: Vec<Recipient>, notification: &Notification);
}
```

这层抽象的价值在于：
- 方便测试（mock dispatcher 永远返回空接收者）
- 方便做方向一/三/四的各种拦截器（组合 `ThrottlingDispatcher` → `CachedPrefsDispatcher` → `DedupDispatcher`）
- 新 Bot 不需要复写接收者展开逻辑

**建议新增：CursorManager 层**

当前游标推进逻辑散布在 `bus.rs`（seq mint）、`delivery_cursor.rs`（游标持久化）、`mod.rs`（replay）。建议收口：

```rust
trait CursorManager {
    fn advance(&self, pid: ParticipantId, room: RoomId, seq: Seq) -> Result<()>;
    fn cursors_for(&self, pid: ParticipantId) -> Result<HashMap<RoomId, Seq>>;
    fn replay(&self, pid: ParticipantId, cursors: HashMap<RoomId, Seq>) -> Vec<RoomEvent>;
    fn gc(&self, older_than: Duration) -> Result<u64>;
}
```

### 3.3 向后兼容性策略

对于所有协议变更（方向二、方向一的通知帧）：

1. **协议字段加而非改**：WS 帧 `kind` tag 不应变化，新增字段用 `Option` 或追加 variant
2. **服务端双通道**：新旧参数并存（`since` + `cursors`），旧参数行为不变
3. **客户端 feature-detection**：`web/ws.js` 在连接时发送客户端版本/能力描述，服务端据此决定使用哪种协议
4. **迁移灰度**：方向二的游标协议应先在 `?cursors=` 参数中探测性支持，先对一个内部工作区启用，验证无误后再全量

当前代码在 `common/src/model/` 中使用 `#[serde(tag="kind")]` 标记枚举——新增 variant 时不会破坏旧 variant 的解析，这是好的。但注意 AGENTS.md §4.2 提过的 `kind` 字段撞名陷阱。

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈？

**短期（当前 sprint）：不需要。**

当前技术栈（Rust + tokio + axum + sqlx + fred/Redis + async-nats + str0m）覆盖了所有核心需求。方向一至五的改进都可在现有技术栈内完成。

**中期评估候选：**

| 领域 | 候选技术 | 评估 |
|------|---------|------|
| 全文搜索 | 已有 pg_trgm + pgvector，自建 hybrid | ✅ 足够。不引入 Elasticsearch |
| 实时推送 | 已有 FCM/APNs | ✅ 互联网推送的正确选择。不引入 WebPush（覆盖面窄） |
| 消息队列 | 已有 NATS JetStream | ✅ 足够。不引入 Kafka/RabbitMQ（运营复杂度高） |
| 可观测 | 已有 Prometheus + OTLP | ✅ 足够。不引入 Datadog（成本高，迁移不必要） |
| 流式媒体 | str0m（纯 Rust DTLS-SRTP） | ✅ 正确的选择。不引入 mediasoup（C++ native 绑定增加部署复杂度） |

**长期值得考察：**

- **消息缓存层**：如果 `dispatch_notifications` 的 per-member 查询成为瓶颈，可考虑引入 `RedisJSON` 或 `Redis Stack` 的文档能力来缓存通知偏好。但这不改变技术栈（fred 已连接 Redis），只是功能拓展。
- **媒体转码**：当前 HLS 切片走 `FlvToTsConverter`（软转码）。如果需 4K 推流或多码率自适应流，应考虑引入 `ffmpeg` 子进程（而非走 rust 绑定）或托管的媒体服务。

### 4.2 第三方依赖评估标准

对于每次新增依赖，建议四维评估（已有的 `deny.toml` 只做许可证检查，不够）：

```
维度 1: 必要性 (1-5)
  此功能是否可以由现有代码实现？如可，成本是多少？
  如引入，减少的代码行数 / 降低的维护成本。

维度 2: 成熟度 (1-5)
  发布历史（≥1 年）
  社区活跃度（近期 commit + issues/PRs 响应）
  关键 bug 的修复周期

维度 3: 风险 (1-5)
  unsafe 代码量（AGENTS.md §4.2 有 `unsafe_code = "forbid"`）
  传递依赖中的 C2PAT / C2PA 等
  二进制大小 / 编译时间影响

维度 4: 维护性 (1-5)
  API 稳定性（semver 遵守）
  文档质量
  作者/组织的信誉
```

分数低于 12/20 的不应纳入。当前项目已无依赖问题（str0m 是纯 Rust、sqlx 编译时安全、fred 是 Rust async-first），但应保持警惕，特别是媒体相关的 C-bindings。

### 4.3 自建 vs 采购

| 领域 | 建议 | 理由 |
|------|------|------|
| 推送网关 (FCM/APNs) | 自建（已有） | 薄网关层，包装成内部 trait，无业务逻辑，不应采购第三方推送平台 |
| AI 审核 | 自建编排（已有） | 调用 Anthropic API，编排逻辑不复杂。不应采购"内容审核 SaaS"——他们通常是黑箱且无法定制预算策略 |
| 实时通话 SFU | str0m 自建（已有） | 纯 Rust 实现，控制力强。不应采购 LiveKit/Agora——成本高、数据出境、不可定制 |
| HLS 转码 | 自建（已有 FlvToTsConverter） | 当前硬编码软转码是正确选择。不应采购 Mux/Encoding.com 等——对直播场景延迟高、成本不可控 |
| 身份认证 | 自建 + OIDC 委托 | 当前 Argon2id + RS256 JWT + OIDC JIT 的混合模式是正确的。不应采购 Auth0/Okta——对自建部署场景成本高、功能冗余 |

**当前"自建"决策都经得起推敲**。核心原则是：核心差异化能力（SFU、AI 预算、通知展开策略）自建，非差异化/合规依赖（OIDC 认证、FCM 推送）使用外部服务但通过 trait 抽象。

---

## 5. 实施路线图

### 5.1 优先级排序

| 方向 | 优先级 | 风险等级 | 业务价值 | 技术复杂度 | 建议时间线 |
|------|--------|---------|---------|-----------|-----------|
| 方向一：@everyone 速率限制 | **P0** | 中（潜在 DoS） | 高（防滥用） | 低（新增守卫函数 + Redis 计数） | 当前 sprint |
| 方向三：全局游标 → per-room 游标 | **P0** | 低（代码已存在一半） | 高（用户体验 + 带宽节约） | 中（协议变更 + 前端适配） | 当前 sprint |
| 方向四：推送风暴控制 | **P1** | 中（但现有未发生） | 中（避免推送通道被封） | 中（限流 + 合并 + 优先级） | 下个 sprint |
| 方向二：通知偏好的批量展开 | **P1** | 低（可选优化） | 中（减少 DB 负载） | 中（Redis 缓存 + CRUD 同步） | 下个 sprint |
| 方向五：流媒体降级策略 | **P2** | 低（现有功能稳定） | 中（提升可靠性） | 高（断流检测 + 连续切片） | 下个里程碑 |
| 媒体 seam 接线（call-bridge） | **P2** | 低（代码已就绪） | 高（真跨节点通话） | 中（仅需真对端验证） | 下个里程碑 |

### 5.2 阶段划分和里程碑

**阶段 I — 安全与质量（2 周）**

目标：消除已知架构债务中最危险的 P0 项。

```
Week 1:
  D1-3: 方向一 — 实现 MentionAmplifierGuard
    - ImService 新增 check_mention_amplification
    - Redis 计数（per-room, per-time-window）
    - dispatch_notifications 调用前守卫
    - 新增 RoomEvent::MentionBlocked
    - 新增迁移：rooms.mention_cooldown_secs / workspaces.max_mention_recipients
    - 冒烟测试：@everyone 在测试房间被限
    - AGENTS.md 更新

Week 2:
  D1-5: 方向三 — per-room 游标协议
    - storage/delivery_cursor.rs: batch_advance + gc + 索引迁移
    - ws/mod.rs: 解析 ?cursors= 参数
    - bus.rs: replay_from_cursors 函数
    - web/ws.js: 连接时发送 cursors 参数（如 localStorage 有）
    - 旧客户端回退：?since 仍工作
    - 冒烟测试：重连后精确恢复
```

**阶段 II — 性能与容量（3 周）**

目标：提升大规模场景下的吞吐能力。

```
Week 3-4:
  方向四：推送风暴控制
    - PushThrottleLayer（Redis 滑动窗口 + 本地 token bucket）
    - 推送合并器（5s 窗口合并多条通知为一条）
    - 优先级通道（DM > @mention > general）

Week 4-5:
  方向二：通知偏好的批量展开
    - NotifPrefCache: Redis HGETALL 缓存通知偏好
    - CRUD 路径同步写入 Redis 和 PG
    - dispatch_notifications 使用缓存批次，而非逐行查询
    - 基准测试：1 万成员频道，消息发布延迟对比
```

**阶段 III — 稳定性与可观测（3 周）**

目标：提升运行期可观测性和边界场景稳定性。

```
Week 6-7:
  方向五：直播降级策略
    - StreamHealthMonitor（断流检测 + grace_period 超时）
    - HLS 连续性（断流输出空 ts + DISCONTINUITY）
    - StreamEvent::Status{Idle} 的超时判断

Week 7-8:
  媒体 seam 接线验证
    - call-bridge 跨节点 e2e 验证（localhost 双实例）
    - 真推流器联调（OBS → RTMP/WHIP → HLS）
    - 可观测性增强：DispatchQueue 滞后监控 + 通知扇出延迟直方图
```

**阶段 IV — 容量测试与文档（2 周）**

目标：验证 10 万成员工作区的运行基线。

```
Week 9:
  负载测试：
    - 100k 成员房间 @everyone + @channel + 普通消息混合
    - 10 万并发 WebSocket 连接
    - NATS 消费者滞后监控（per-room subject）
  瓶颈分析与优化

Week 10:
  文档与可维护性
    - docs/architecture/ 下补充架构决策记录
    - 更新 AGENTS.md 新增智能体（如有）
    - 整理方向一/三/四的 CLI 配置项文档
    - 发布阶段 I-III 的性能基准报告
```

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| 方向一：过多拦截导致 @everyone 功能几乎不可用 | 中 | 高（用户抱怨） | 配置默认值应保守（max_mention_recipients=自动检测房间规模×2，cooldown=30s），提供管理员面板调控 |
| 方向三：游标表膨胀至不可查询 | 中 | 高（慢查询回退新连接） | 游标 GC 必须在启用前先运行一次完整清理；建议先跑预迁移加索引，再启用协议 |
| 方向四：推送合并导致消息延迟显著增加 | 低 | 中（用户体验） | 合并窗口应动态调整（设备在线→5s，不在线→立即推）；DM 消息跳过合并窗口 |
| 方向二：Redis 缓存与 PG 不一致 | 中 | 中（错过推送或重复推送） | 使用 Redis 作为缓存（读时同时查 Redis+PG 验证），而非权威数据源；不一致时以 PG 为准并修复缓存 |
| 阶段 III：真推流器联调发现 str0m 版本兼容问题 | 中 | 高（block 发布） | 阶段 II 末尾就启动 pre-alpha 联调，不要等到阶段 III 中间才第一次连接 OBS |
| 媒体 seam：call-bridge 的 `AERO_INTERNAL_BRIDGE_SECRET` 泄漏 | 低 | 极高（任意节点可接管 SFU） | 应限制 bridge subscribe endpoint 仅 from cluster internal IPs（iptables / 网络策略），不只依赖 secret |

---

## 总结

Aero IM 整体架构稳健，分层合理，事件驱动架构建模得当。主要架构债务集中在**大规模扇出的无防护**（@everyone 放大器、推送风暴、通知偏好 O(N) 查询）和**协议级的半截工程**（delivery_cursor 表存在但未使用）。

建议逐步夯实：

1. **先止血**（P0：@everyone 限流 + per-room 游标）——这两项一个防 DoS，一个改善用户体验，且实施成本低
2. **再健体**（P1：推送流控 + 通知偏好缓存）——提升大规模运营的可靠性基线
3. **后强壮**（P2：直播降级 + 媒体 seam 验证）——完善边界场景，让已有代码产出价值

整体"自建核心、trait 抽象外部依赖"的技术策略经得起推敲，不需要引入新的技术栈或框架。
