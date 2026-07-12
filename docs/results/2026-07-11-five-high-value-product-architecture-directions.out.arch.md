以下是我的架构分析。

---

# 架构分析：Aero IM 的 5 个高价值扩展方向

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 的架构在 Rust 生态中做出了几个**教科书级的设计决策**：

**事件 DAG 的物理分层正确。** 系统将「跨实例事实源」（NATS JetStream at-least-once）与「进程内扇出」（Hub bounded mpsc）明确分离。这意味着每实例的 `run_bus_listener` durable consumer 是集群级状态复制的唯一点，而 `Hub::fan_out_raw` 只负责本地扇出。这两个关注点的物理分离使得：

- 水平扩展时不会放大 NATS 订阅开销（每个 subject 每实例只有一个 consumer）
- 慢消费者背压被隔离在本地 bounded channel 内，不传播到 NATS ack 循环
- 丢失 seq 的帧可被 `SeqGate` 在进程入口拒掉，但不会污染其他实例的状态

这是一个**生产级的事件驱动架构**应该有的样子——绝大多数 Rust 项目的事件系统只做到「tokio::broadcast 本地扇出」就停了。

**媒体面与信令面正交。** WebRTC 的 ICE/DTLS/SRTP 握手（str0m）与业务信令（NATS + WS）通过 `CallRoster` / `SfuRouter` 两个独立的状态域交互，不共享锁。`SfuMediaSession` 的 `on_rtp` → `SfuForwarder` → `CallEgress` 管线是一个纯粹的数据流编排，不涉及业务逻辑。

**限流的三层架构（本地 DashMap → WS 速率 → Redis 集群窗口）**在 Rust 生态中属于深思熟虑的设计。`ClientKey::Participant` 优先于 `ClientKey::Ip` 表明设计者理解：在 NAT/反向代理场景下，IP 是比 participant ID 远不可靠的限流键。

**事务与幂等的分层承诺清晰。** `AGENTS.md` §4.2 明确列出「at-least-once 状态机」的工程约束——付费/外发/计费有幂等键，fail-open/fail-closed 行为在文档级有标注。这些约束比大多数系统在架构文档中写的东西多。

### 1.2 当前架构的局限性

**领域事件膨胀到 `RoomEvent` 单体枚举。** 目前 `RoomEvent` 包含 16+ variant（Message, Edited, Deleted, Reaction, Read, Typing, Notify, NotifyBatch, Pin, Membership, Call, Poll, MessageSeen, Interaction...），且通过 `tag="kind"` 序列化。每个新功能都必须：

1. 在 `RoomEvent` 加 variant（当心 `kind` 字段名冲突陷阱）
2. 在 `room_event_to_frame_json` 加 match 臂
3. 在 web 端加 `ws.on('msg:..')` 处理

这是个**已识别的架构债务**——单体事件枚举在 16 variant 时还可管理，但进入 30+ variant 后将出现：
- 编译时间增加（每次修改触发全量重新编译 `aero-common`）
- 模式匹配遗漏风险（`#[non_exhaustive]` 可缓解但 Rust 编译器会给下游 crate 警告）
- 序列化分支的可测试性下降

**前端无状态管理框架，但 `state` 对象已膨胀到单体。** `context.js` 的 `state` 对象是一个全应用共享的可变全局，所有模块通过 `import { state }` 共享。这在前端规模较小时（~3K LOC）足够，但按照 AGENTS.md 的功能矩阵（搜索/AI/通话/直播/投票/通知/线程），`state` 的模式匹配和隐式依赖关系将不可维护。

关键信号：`calls.js` 直接修改 `state.activeCall`，`notifications.js` 修改 `state.notifications`，`live.js` 修改 `state.watchedStreams`——没有模块间的事件通知机制。一个模块修改了状态后，其他模块依赖的 UI 是否更新取决于渲染循环的触发时机（`renderMessage` vs `refreshRoomsFromServer`），没有确定性保证。

**跨 crate 的 `RoomEvent` 耦合。** `aero-common` 定义了 `RoomEvent`，但 `aero-im-core/src/service/events.rs` 发布它，`aero-server/src/ws/frame.rs` 消费它翻译为 `ServerFrame`。这形成一个**编译时依存链**：ws 模块依赖 common 的事件定义。这是预期的（crate 引用方向是向下的），但意味着 ws 协议与业务事件绑定——如果要向前端发一个「非业务事件」的控制帧（如你现在建议的 `sync_state`），必须在 `ServerFrame` 加新的 variant，而不是复用 `RoomEvent`。

**这是你原文分析中未指出的一个架构约束**：`ServerFrame` 与 `RoomEvent` 的生硬映射。看 `room_event_to_frame_json` 的 `NotifyBatch` 分支——它把 batch 展开了成单个 `Notify`，这意味着批量通知的原始结构丢失了。如果你未来需要前端做批量通知的去重或折叠，这个展开是信息丢失。

### 1.3 关键设计决策评估

| 决策 | 评价 | 风险 |
|------|------|------|
| NATS 作为唯一事实源 | ✅ 正确。更适合跨语言/跨平台的事件持久化 | JetStream 的 GC 策略影响历史回放时长 |
| Hub bounded channel 扇出 | ✅ 正确。背压隔离的关键 | `disconnect_on_full` 的丢帧率可观测性不足 |
| `SeqGate` 帧级去重 | ✅ 正确。at-least-once 的基础 | 仅帧级去重，不覆盖消息持久化前重复 |
| 状态管理无框架 | ⚠️ 接受。适合当前规模 | 按功能矩阵扩展后将到极限 |
| `CallRoster` 走 Redis 而非进程内存 | ✅ 正确。跨节点通话必需 | Redis 故障时的 fail-open 行为未验证 |
| 前端无测试 | ❌ 不可接受。参见方向四 | UI 回归在 CI 中零检测 |

## 2. 扩展方向（高价值架构方向）

我的分析从原始文档的 5 个方向出发，但基于代码验证后的调整，重新组织了优先级和实现策略。以下是 5 个高价值方向，按我评估的**实际优先级**排序。

### 方向一：消息投递端到端幂等（P0）

**为什么需要。**

原始文档的分析正确但遗漏了一个关键事实：**`StreamGift` 已有 `nonce` 幂等键（第 195-200 行）**。这意味着系统内已有幂等键的设计模式和数据库模式。这不是引入新机制，而是**将已验证的模式泛化到 `SendMessage` 和 `SendMarkdown`**。

但问题不仅仅是加一个字段。更深层的架构问题是：**消息投递的 at-least-once 保证依赖于两层不同的幂等语义**：

1. **帧级**（`SeqGate`）：保证同一 bus seq 不扇出两次——不覆盖消息持久化前的重复
2. **消息级**（未来）：保证同一 `idempotency_key` 不插入两条——覆盖重投和重连场景

这两层各自的失效模式不同：

| 层 | 失效条件 | 后果 |
|---|---|---|
| `SeqGate` | 进程重启（state 丢失） | 重启后首次收到的 seq 重新接受 |
| 消息级幂等键 | 客户端未设置 `idempotency_key` | 约退到当前行为（有重复风险） |

所以**幂等键不是银弹**——它的有效性依赖于客户端的合规实现。如果第三方客户端不设 `idempotency_key`，重复仍可发生。

**核心架构挑战。**

1. **数据库层的唯一约束如何不成为写入瓶颈**。`ON CONFLICT DO NOTHING` 在正确索引下开销可接受，但 `idempotency_key` 列的索引增加了 `messages` 表的写入放大。如果 `idempotency_key` 可为 NULL（迁移前消息），唯一索引必须用 `NULLS NOT DISTINCT`（PG 15+）或分区索引。

2. **幂等键的生存期。** 消息投递的幂等键应该在何时被清理？保留永久会膨胀 `messages` 表；清理太早又可能在长时间断连后失去去重能力。一个合理的折中是：幂等键的保留期 = 消息的 retention 周期（由 channel retention 策略决定）。

3. **`SendMarkdown` 路径必须同期覆盖。** 这是你验证结果中的关键校正——`SendMarkdown`（第 96-102 行）进入相同的消息持久化路径，如果它不设幂等键，攻击者可以避开消息幂等约束。

**预期的架构变更。**

```
ClientFrame::SendMessage {
    room_id: RoomId,
    blocks: Vec<Block>,
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
    #[serde(default)]
    nonce: Option<String>,          // ← 新增，复用 StreamGift 的 field 名
}
ClientFrame::SendMarkdown {
    room_id: RoomId,
    markdown: String,
    reply_to: Option<MessageId>,
    expires_after_secs: Option<u64>,
    #[serde(default)]
    nonce: Option<String>,          // ← 新增，同上
}
```

迁移：
```sql
ALTER TABLE messages ADD COLUMN IF NOT EXISTS idempotency_key TEXT;
-- 唯一索引（允许 NULL，NULL = 迁移前消息不过问）
CREATE UNIQUE INDEX IF NOT EXISTS uq_messages_idempotency
    ON messages (idempotency_key) WHERE idempotency_key IS NOT NULL;
```

**对现有系统的影响。**

- 旧客户端不设 `nonce` → 降级到当前行为（无幂等）。201 兼容。
- 幂等键仅在 `ON CONFLICT DO NOTHING` 命中时产生数据库写入开销（通常 <1% 的写入）。
- web 端 `ws.sendMessage` 参数签名加可选 `nonce`——当前调用者不改也行。
- 幂等键本身是 ULID（客户端生成，含时间戳），可兼作客户端侧的 created_at。

### 方向二：前端状态一致性协议（P1，重定义为务实版本）

**为什么需要。**

原始文档建议 IndexedDB + Service Worker。这是正确但不务实的起点。让我解释为什么：

**IndexedDB 对于 Aero IM SPA 来说是架构级变更**，原因有三：

1. **零构建工具的 ES2020 模块不能方便地引入 IndexedDB 封装层.** 当前 `context.js` 的 `state` 对象是同步全局可变引用——要把它改成持久化 + 异步 + 竞争条件感知，相当于重写 SPA 的状态管理。
2. **IndexedDB 容量限制明显.** 移动 Safari 约 50MB 上限，消息体 + Block 元数据 + 附件引用在活跃用户的几个房间里就能达到。
3. **Service Worker 增加调试复杂度.** Service Worker 的生命周期（install → activate → fetch → message）在离线/在线切换时的竞争条件是已知的 "PWAs hardest bug"。

**更务实的起点**：利用现有机制——`sessionStorage` + 重连检测 + 增量同步。

**核心架构挑战。**

1. **写后读验证（Read-Your-Writes）的时机模型。** 用户发送消息后，UI 立即乐观渲染。服务端回显到达时（通过 WS 帧），匹配 `pendingByTempId` 替换为真实消息。但这里有一个**时序窗**：回显未到达前用户切换房间再切回来，乐观消息丢失。当前的 `?since=` 回放会重放服务端的真实消息，但不覆盖「用户认为已发送但服务端未收到」的消息。

2. **`sync_state` 全量快照协议与增量推送之间的交互。** 如果 `sync_state` 返回全量状态快照，客户端必须能合并增量推送和全量快照——快照可能已过时（从服务端发出到客户端接收之间发生了增量事件）。这是分布式状态同步的经典问题，解决方案通常是：快照 + 增量事件日志的序列号，客户端合并两者。

**建议的架构变更（务实版）。**

```
// 新 WS ClientFrame variant
SyncState { rooms: Vec<RoomId> }

// 新 WS ServerFrame variant
StateSnapshot {
    rooms: Vec<RoomSnapshot>,      // 每个房间的摘要
    receipts_by_room: HashMap<RoomId, ReceiptCursor>,
    reactions_by_msg: Vec<ReactionSnapshot>,
    online_by_room: HashMap<RoomId, Vec<ParticipantId>>,
    seq: u64,                      // 全局单调递增 seq，客户端用来推断增量丢失
}
```

客户端合并规则：
- 收到 `StateSnapshot` 后，**原子替换** rooms/receipts/reactions 状态
- `online_by_room` 是全量快照（非增量），所以之前的增量 presence 帧全部覆盖
- `seq` 用于检测「快照生成后到应用前是否有增量帧到达」——若有，等待增量帧处理完再替换

**对现有系统的影响。**

- 不需要新数据库查询：`sync_state` 所需数据全部可以从现有 REST 端点组合（`GET /api/rooms` + `GET /api/receipts` + WS 中的 presence 快照）。
- 服务端增加的负担：序列化全量快照的 JSON 大小 ≈ 房间数 × (200B + 在线成员数 × 20B)，100 房间 × 50 在线成员 ≈ 120KB——一次性的，在 WS 通道上传输 <10ms。
- web 端 `state` 对象加 `_snapshotSeq` 字段，`handleIncomingFrame` 中增量更新时更新 seq，`syncState` 替换时校验 seq。

### 方向三：弹性限流的反馈回路闭环（P1）

**为什么需要。**

原始文档和验证结果都确认了核心问题：**服务端限流了，但客户端忽略响应头，导致限流触发后的行为不可控。** 这是系统韧性的一个「开环」——输出动作（429）不被输入端（客户端）解析，所以没有负反馈收敛。

更严重的是：**ws.js 的重连退避是硬编码数组，且最后一步无 jitter——30 秒后全量客户端齐发重连，造成恢复风暴。** 这在服务端重启后必然发生（NATS consumer 重连 + Hub 重建）。

**核心架构挑战。**

1. **客户端退避策略的「谁决策」问题。** 退避延迟可由客户端自行计算（指数退避 + jitter），或从服务端 `Retry-After` 头学习，或两者组合。选择方案：
   - **纯客户端退避**：简单，但无法响应服务端负载变化（服务端已过载，客户端仍按固定退避重试）
   - **服务端指导退避**：`Retry-After` 头 + WS 恢复帧嵌入 `after` 建议——更精确，但增加服务端逻辑
   - **组合方案**：客户端退避为基线，`Retry-After` 覆盖基线

   **建议：组合方案**。客户端维护 per-endpoint 的 `nextRetryAt` 时间戳，首次 429 时退避 `min(Retry-After, 5s)`，后续指数增长到 60s cap + jitter。

2. **`sweep_idle` 的安全 GC 策略。** 你验证结果中关于 sweptle 的分析比我原文更细。当前 `sweep_idle` 只回收 `tokens >= capacity && elapsed >= idle_after` 的 bucket。这确实是低烈度 DoS 放大而不是内存泄漏——但低烈度不等于可忽略。

   **建议：`sweep_idle` 增加 `MAX_IDLE` 参数（默认 300 秒），不管 token 水位，只要 `elapsed >= MAX_IDLE` 就回收。** 这是「可用性优先于严格限流」的权衡——一个 300 秒前耗尽 token 的客户端，现在回来发送请求，即使不准确地放行几个请求，也比永久占用 DashMap entry 好。

3. **`resync` 恢复风暴的速率控制。** `hub.rs` 发送 `RESYNC_FRAME` 时，所有慢消费者同时收到信号，同时发起 REST 回拉。这是一个隐式的**雷鸣群问题**。

   **建议：`RESYNC_FRAME` 嵌入 `after: u64` 字段（建议等待秒数），客户端收到后延迟 0~after 之间的随机时间再发起 REST。** 这要求 `hub.rs` 在发送 `RESYNC_FRAME` 时知道当前挂起的慢消费者数量——如果多，增加 `after`。

**对现有系统的影响。**

- `api.js` 的 `request()` 函数加 `# 429 处理中间件`
- `ws.js` 的 `BACKOFF_MS` 替换为自适应退避 + jitter
- `rate_limit.rs` 的 `sweep_idle` 加 `max_idle` 参数
- `hub.rs` 的 `RESYNC_FRAME` 加 `after` 字段（JSON 扩展，旧客户端忽略即可）
- 无数据库变更，无新 crate 依赖

### 方向四：故障注入与混沌就绪度（P1，与方向一绑定）

**为什么需要。**

这是原始文档分析得最透彻的方向，但你的验证结果补充了一个关键发现：**方向一和方向四的优先级已耦合**。消息幂等的 at-least-once 护栏只有在故障注入测试中才能得到验证。没有故障注入测试，方向一的 `ON CONFLICT DO NOTHING` 分支在 CI 中永远不会被执行。

这是**所有 at-least-once 系统的共性陷阱**：幂等键的正常路径永远正确（多次调用 INSERT，唯一约束阻止重复），但「重投触发幂等键命中」的唯一途径是让消息在消费过程中失败。这在单元测试中模拟困难，因为需要精确控制 NATS 的 ack 时序。

**核心架构挑战。**

1. **Mock 化的边界在哪。** 为测试故障路径，需要让 `aero-storage`、`aero-bus`、`Hub` 等在测试中可注入故障。当前设计使用 trait（`EventBus`、`BlobStore`），这是好的起点。但 `PgPool` 是具体类型（`sqlx::PgPool`），不能在测试中「让某次查询超时」。解决方案有两个方向：

   - **方案 A**：在仓储层加故障注入 wrapper（`FaultyRepo<T>`），包裹真实的 repo，在配置的概率/次数下模拟失败。优点是测试代码少，缺点是运行在真实 DB 上（事务隔离的副作用不能模拟）。
   - **方案 B**：用 `testcontainers` 起真实 PG/Redis/NATS，用 `iptables` / `tc` 引入网络故障。优点是真实度高，缺点是 CI 耗时较长。

   **建议：方案 A 作为起点，方案 B 作为季度性的混沌演练。** 方案 A 可以集成到常规 CI（~3 min），方案 B 作为每晚的 `make test-chaos`。

2. **Fail-open 路径的观测性。** 当前 `tracing::warn!` 是 fail-open 的主要日志手段。但 `warn!` 在正常运维中被视为「需要注意但不紧急」——没有 `tracing::error!` 来触发告警。这意味着一个 fail-open 路径被触发时，运维人员只有在主动查看日志时才可能发现。

   **建议：为每个 fail-open 路径引入双轨日志**——一次 `tracing::warn!`（描述降级行为）+ 一个 Prometheus 计数器增量（`aero_failover_total{component="rate_limit",strategy="allow_on_redis_timeout"}`）。这样，异常行为在监控仪表盘上立即可见，而告警规则可以基于计数器增长率触发。

3. **前端 fail-open 的无测试盲区。** 你验证结果中提到的 `catch { return; }` 模式在 `web/` 下的散布是最危险的盲区。JS 的异常静默吞掉意味着用户看到的是 UI 不更新但无错误提示——这是比服务端 fail-open 更难调试的问题。

   **建议：引入 `window.onerror` / `window.onunhandledrejection` 全局处理器，在开发模式下弹 toast，在生产模式下向 `/api/log` 发结构化错误报告。** 这将把前端 fail-open 从「静默失败」转化为「可观测的降级」。

**对现有系统的影响。**

- 无新 crate。故障注入 wrapper 放在 `aero-storage/src/test_util/`。
- 现有仓储 trait 不需要修改，FaultyRepo 通过包裹已有 impl 工作。
- CI 加一个 `make test-negative` 步骤（默认并行于主测试套件）。
- 前端 `api.js` 加 `initGlobalErrorHandler()`。

### 方向五：媒体面生产化（P2，但识别为「必需的 seam」）

**为什么需要。**

原始文档对这个方向的分析准确但有一个需要补充的视角：**媒体面不是一个「可选的」产品质量问题——它是 Aero IM 核心价值主张的实体化。** 如果客户不能通过 WHIP 推流直播、不能通过 SFU 进行群组通话、不能通过 call-bridge 跨节点通话，那么 Aero IM 的 AI-Native IM + 直播定位就只是 README 上的文本。

但更重要的是：**媒体面有三个天然的复杂性盲区，单元测试无法覆盖：**

1. **时钟同步**：`SfuMediaSession` 的 `run` 循环依赖 tokio 的时间——`tokio::time::interval` 在 CI（可能高负载）下的行为与生产环境不同。媒体时间戳的处理（`SfuForwarder` 的 seq/ts 重映射）在单元测试中用 `Instant::advance` 模拟，但这覆盖不了真实时钟漂移。

2. **编解码器协商**：WHEP 路径中，浏览器期望的 H.264 配置（profile-level-id、packetization-mode）与服务器 depacketizer 输出的配置必须精确匹配。单元测试用硬编码 SDP，但真实浏览器的 SDP offer 因操作系统/浏览器版本/硬件编码能力而异。

3. **UDP 流量下的套接字行为**：`call_bridge_supervisor` 使用 `UdpSocket`。单元测试中 UdpSocket 在 localhost 上工作完美，但生产环境中的 NAT 绑定、MTU 分片、路由器缓冲区膨胀会导致仅在长期运行中显现的问题。

**核心架构挑战。**

1. **CI 沙箱的媒体验证成本。** 在 CI 中使用真实的 WebRTC（浏览器 ↔ str0m）需要浏览器运行时。传统方案（Playwright + Chromium）意味着 CI 环境需要 Xvfb + Chromium + ALSA 虚拟音频设备，这是一个约 500MB 的依赖安装。对于纯 Rust 项目来说，这个依赖栈的门槛不低。

   **建议：分两阶段。**
   - **阶段一（当前可做）**：用 ffmpeg 模拟推流。ffmpeg 可以生成 RTP 流（通过 WHIP 的 HTTP 端点）和 RTMP/SRT 流。ffprobe 验证 HLS 输出的编码格式。这个路径不需要浏览器，CI 中安装 ffmpeg 的代价很小。
   - **阶段二（后续）**：用 Playwright + Chromium 做真正的 WHIP/WHEP E2E，验证 DTLS 握手和 RTP 吞吐量。

2. **call-bridge 的多节点测试。** 在 CI 中启动两个 aero-server 实例 + 两个 NATS + 两个 Redis + 两个 PG 是一个复杂度挑战。Docker Compose 是合理选择，但需要确保测试环境不被宿主机上的 Docker 资源竞争影响。

   **建议：单机双进程模式。** 启动两个 aero-server 实例，绑定不同端口（3030 和 3031），使用共享的 NATS/Redis/PG。call-bridge 的 `ensure_bridges` 逻辑会连接到另一个实例的 bridge 端口。单机 localhost 的 UDP 往返是真实网络路径的合理近似。

**对现有系统的影响。**

- 无代码变更。所有媒体组件已在 AGENTS.md §2 标记 seam 状态。
- 需要 Docker Compose 配置（`docker-compose.ci.yml`）用于 CI 沙箱。
- 建议在 README 或 SPEC 中为每个媒体组件标注联调状态（✅ 单测 / 🔶 真实联调中 / ❌ 未验证）。

---

## 3. 接口设计建议

### 3.1 关键模块的接口设计原则

**原则一：幂等键是基础设施，不是业务字段。**

幂等键 `nonce` 不应出现在 `Message` 数据模型中，而应在传输层（`ClientFrame`）和仓储层（WHERE 子句）中。理由：

- 幂等键不在消息的 REST 响应体中暴露（客户端不需要看到别人消息的 nonce）
- 幂等键在 `ON CONFLICT DO NOTHING` 后不返回冲突行——这意味着调用方（`ImService::send_message`）的返回值在幂等命中时应该是「返回已存在的消息」，而不是「返回错误」

这个区分决定了接口设计：

```rust
// 正确抽象
pub enum SendResult {
    Created(Message),          // 首次投递成功
    Duplicate(Message),        // 幂等键命中，返回已存在的消息
}

impl ImService {
    pub async fn send_message(
        &self,
        participant_id: ParticipantId,
        room_id: RoomId,
        blocks: Vec<Block>,
        nonce: Option<String>,  // ← 传输层参数，不入 Message
    ) -> Result<SendResult, Error>;
}
```

**原则二：ServerFrame 与 RoomEvent 解除强绑定。**

当前 `room_event_to_frame_json` 是一个 16 臂的 match 表达式，占 ~50 行。每次加 `RoomEvent::Variant` 必须同时加 WS 翻译。更好的模式是：**让 `ServerFrame` 变成一个完全的独立协议层**，而不是 `RoomEvent` 的序列化映射。

```rust
// 当前：强绑定
ServerFrame::Message { message: env.message }  // 直接从 RoomEvent 提取

// 建议：独立的 WS 协议定义
pub enum ServerFrame {
    Message {
        id: MessageId,
        room_id: RoomId,
        sender: ParticipantId,
        blocks: Vec<Block>,
        // ...显式列出所有 WS 需要的字段
    },
    // 每个 variant 独立于 RoomEvent
}
```

这意味着消息处理管线变成：

```
RoomEvent::Message(env) → ws_translator::message_to_frame(env) → ServerFrame::Message{...}
```

好处是 WS 协议的修改不影响业务事件定义，且可以为 WS 协议单独写序列化测试。代价是多一个转换层——对于当前规模可接受。

**原则三：限流中间件的返回格式与客户端解析器对称。**

不要把 429 的 body 格式当成纯后端决策——它定义了客户端解析器的契约。我建议：

- 429 body（如需要）：`{ "error": "rate_limited", "retry_after": 1 }`（JSON，最少字段）
- 保留现有的 `X-RateLimit-*` 头作为首选消费接口
- `api.js` 的 `request()` 在收到 429 时，先读 `Retry-After`，取其值；若无头则 fallback 到响应 body 的 `retry_after`；若两者皆无，用默认值 `1`

这样，为带宽敏感的路径（如 STATIC 资源）保留纯 header 响应的灵活性，而常规 API 路径也提供结构化 body。

### 3.2 是否需要引入新的抽象层

**是的，需要两个抽象层：**

**抽象层一：前端状态同步协议。** 当前前端消息到达的路径是 `ws.on('message')` → `handleIncomingMessage()` → 直接修改 DOM。这是命令式渲染，不适合长期扩展。引入一个薄的「状态同步 → 渲染」层：

```
WS ServerFrame → state.apply(frame) → DOM diff update
```

这不需要引入 React/Vue——可以用一个简单的发布订阅模式实现：

```javascript
// 不引入框架，只引入模式
class Store {
    constructor(initial) { this._state = initial; this._listeners = new Map(); }
    get state() { return this._state; }
    apply(update) { /* 合并更新 */ this._emit('change', this._state); }
    on(event, fn) { this._listeners.set(event, fn); }
}
```

当前 `context.js` 的 `state` 对象可以直接包装进 `Store`——不需要改变所有模块的引用方式。

**抽象层二：故障注入 wrapper。** 在仓储层引入一个通用的 `FaultyMiddleware` trait：

```rust
#[cfg(test)]
pub struct FaultyRepo<R: Repo + Send + Sync> {
    inner: R,
    fault_config: Arc<FaultConfig>,
}

#[cfg(test)]
impl<R: Repo + Send + Sync> FaultyRepo<R> {
    pub fn new(inner: R, config: FaultConfig) -> Self { ... }
}

pub struct FaultConfig {
    pub fail_probability: f64,       // 0.0 ~ 1.0
    pub delay_probability: f64,      // before response, add random delay
    pub delay_range: Range<Duration>,
    pub error_kind: ErrorKind,       // which Error variant to return
}
```

这样，任何已有 trait `Repo` 的仓储（`MessageRepo`、`ParticipantRepo` 等）都可以被包裹，不需要修改实现。

### 3.3 向后兼容性

所有建议的变更都应该是**增量部署安全的**：

- `ClientFrame::SendMessage` 新增 `nonce: Option<String>` 是 `#[serde(default)]`——旧客户端发来的帧不变
- `RESYNC_FRAME` 加 `after` 字段——旧客户端忽略额外字段（serde `deny_unknown_fields` 未启用）
- `messages` 表加 `idempotency_key` 列——NULL 兼容，唯一索引只覆盖非 NULL 行
- 前端 `request()` 加 429 中间件——旧代码的 `throw new ApiError(429)` 不受影响，中间件在抛出异常前处理

唯一的不兼容变更可能来自 `ServerFrame` 协议扩展——如果新 variant（如 `StateSnapshot`）被旧客户端接收，旧客户端的 `ws.on('message')` 的 switch 语句没有对应 case → 帧被静默丢弃（`return` 不处理）。这对旧客户端**不是错误**（未识别的 `"type"` 被忽略），但意味着旧客户端看不到新功能。在 ES2020 模块的更新机制中，客户端每次刷新页面获取最新 JS，所以兼容窗口只有用户刷新的间隔。

---

## 4. 技术选型

### 4.1 无需引入新技术栈

我评估了 5 个方向后认为：**当前技术栈不需要新的运行时依赖。** 具体来说：

| 方向 | 是否需新依赖 | 理由 |
|------|-------------|------|
| 幂等键 | 否 | 复用现有 serde + sqlx + ULID |
| 前端状态一致性 | 否 | `sessionStorage` + ES2020 `Proxy` 或自定义 `Store` 类，零依赖 |
| 弹性限流 | 否 | 纯 `api.js` 改动 + `rate_limit.rs` 参数调整 |
| 故障注入 | 可选 | `testcontainers`（已有）或用纯 Rust 的 mock。推荐后者（零 Docker） |
| 媒体面 E2E | 是 | ffmpeg（CI 中安装）— 但 ffmpeg 是 CI 依赖不是运行时依赖 |

这意味着 Aero IM 的**运行时依赖的 footprint 不增长**。这是 Rust 生态的一个重要优势——你不需要引入一堆 JS 库来实现前端状态管理或 HTTP 重试。

### 4.2 关于前端框架的审慎考虑

当前 SPA 使用零依赖 ES2020。按功能矩阵的扩展速度，我评估：

- **3K LOC**（当前）：纯 ES2020 足够
- **5K LOC**（3-6 个月）：需要 `Store` 模式（上述的自定义发布订阅）
- **10K LOC**（9-12 个月）：需要考虑引入 web component 框架（Lit 或 `HTMLElement` 子类化）

**为什么要延迟引入框架？** 因为引入构建工具链（Vite/Webpack）将打破当前的「刷新即更新」模型——用户需要在首次访问后重新下载编译后的 bundle，而不是每次刷新获取最新 ES module。对于 Aero IM 的快速开发节奏，零构建是一个被低估的优势。

### 4.3 关于 Testcontainers 的评估

故障注入方向（方向四）需要做 mock。两个路线：

| 方案 | 优点 | 缺点 |
|------|------|------|
| Pure Rust mock | 无 Docker 依赖，CI 快 | 不能模拟真实网络故障；`sqlx::PgPool` 不易 mock |
| `testcontainers` | 真实 PG/Redis/NATS；可注入 iptables 规则 | CI 慢（5-10 min）；CI runner 需要 Docker |

**建议：两阶段。** 阶段一（当前 CI）用 Pure Rust mock，覆盖「数据库查询返回 Err」这类的简单故障。阶段二（季度混沌演练）用 testcontainers 加网络故障注入。阶段一的工装（`FaultyRepo`）可以无缝升级到阶段二，`FaultConfig` 的配置参数不变。

### 4.4 自建 vs 采购的决策依据

| 组件 | 决策 | 理由 |
|------|------|------|
| 幂等键 | 自建（~50 行 Rust） | 数据库层 ON CONFLICT DO NOTHING 是最简洁的幂等实现 |
| 前端状态同步 | 自建（~100 行 JS） | 协议简单（WS JSON），没有商业或开源方案直接匹配 |
| 限流客户端退避 | 自建（~30 行 JS） | 标准模式，无外部依赖 |
| 故障注入框架 | 自建（~200 行 Rust） | Rust 生态中无现成的「仓储层故障注入」库 |
| 媒体 E2E 测试 | 用现有工具（ffmpeg + Playwright） | 不需要自建测试框架 |

**不需采购任何商业产品**。以上所有方向都可以在现有技术栈内实现。

---

## 5. 实施路线图

### 优先级排序

| 方向 | 优先级 | 风险 | 投入 | 前置依赖 |
|------|--------|------|------|----------|
| **一：消息幂等** | P0 | 数据正确性，用户信任 | 2-3 天 | 无 |
| **二：前端一致性（务实版）** | P1 | 用户体验 | 3-4 天 | 无 |
| **三：限流反馈回路** | P1 | 运营韧性 | 2-3 天 | 无 |
| **四（阶段一）：故障注入 + fail-open 审计** | P1 | 运营成熟度 | 4-5 天 | 方向一（需要幂等键来验证 at-least-once） |
| **五（阶段一）：ffmpeg 媒体 E2E** | P2 | 产品质量 | 3-4 天 | 无 |
| **四（阶段二）：混沌 CI 演练** | P2 | 运营成熟度 | 5-8 天 | 方向四阶段一 |
| **五（阶段二）：Playwright WHIP/WHEP E2E** | P3 | 产品质量 | 5-10 天 | 方向五阶段一 |

### 阶段划分

**阶段 0（紧前，第 1-2 天）— 基础设施就绪**

- 方向四的 fail-open 审计（零代码工作）：列出所有 `tracing::warn!` 路径，确认日志格式一致性、Prometheus 计数器是否就位
- 方向二的 `sessionStorage` 缓存：房间列表、已读游标——KB 级，2 小时完成
- 这两项**不依赖对方**，可并行

**阶段 1（第 3-5 天）— 消息幂等 + 限流闭环**

- 方向一实现：`ClientFrame` 加 `nonce`、`messages` 表加列、`ImService::send_message` 加幂等分支
- 方向三实现：`api.js` 429 中间件 + `ws.js` 自适应退避
- 方向一的 web 端改动（`ws.sendMessage` 参数加 `nonce`）与方向三的 `api.js` 修改在同一文件，建议一次发布

> **并行风险**：方向一需要数据库迁移（`ALTER TABLE messages ADD COLUMN`），方向三不需要。先做迁移 build → migrate，再做方向三。两次发布是安全的。

**阶段 2（第 6-10 天）— 前端一致性 + 故障注入**

- 方向二：`sync_state` 协议 + `Store` 模式 + 重连后全量快照
- 方向四阶段一：`FaultyRepo` + 5 个核心故障注入测试用例（Redis timeout、NATS disconnect、DB query error、blob store unavailable、AI key missing）

> **并行风险**：方向二和方向四的代码改动完全不重叠（前端 vs 后端测试基础设施）。可完全并行，但需要确保方向一的幂等键在测试中可被故障注入触发（这是方向四的主要验证目标之一）。

**阶段 3（第 11-15 天）— 媒体面 E2E**

- 方向五阶段一：ffmpeg 推流 E2E 测试 + 媒体面健康自检
- 如果时间和资源允许，开始方向五阶段二（Playwright WHIP/WHEP）

### 风险点和缓解策略

| 风险 | 影响 | 概率 | 缓解 |
|------|------|------|------|
| `send_message` 幂等键分支在 `ON CONFLICT` 的并发下的正确性 | 幂等键失效，消息重复 | 低 | 加 `FOR UPDATE` 锁在幂等键检查前？不——`ON CONFLICT DO NOTHING` 是原子操作，不需要额外锁。风险在于返回的 `Message` 行可能不是调用方预期的 content。缓解：`SendResult::Duplicate` 永远返回数据库中的当前行（可能是编辑后的版本），调用方确认这一语义 |
| 前端 `sync_state` 快照时序 | 快照到达前有增量帧，快照覆盖了增量 | 中 | `StateSnapshot` 带 `seq`，客户端在快照到达后检查 `state._currentSeq` 是否大于快照 seq——若否，快照可安全应用；若是，需要合并或丢弃快照重请求 |
| 429 中间件误判 | 非限流错误（503）也被退避 | 低 | 只在 `status === 429` 时触发退避，其他错误正常传播 |
| `FaultyRepo` 影响测试确定性 | CI 中随机失败 | 中 | `FaultConfig` 使用固定的种子（`StdRng::seed_from_u64`），使故障注入具有确定性；每次 CI 运行使用相同的系统时间/种子 |
| ffmpeg 媒体 E2E 在 CI 中的编解码器兼容性 | HLS 输出格式与断言不一致 | 低 | 使用固定版本的 ffmpeg（Docker 中的 `jonasal/ffmpeg`），且输出断言只检查编码格式、关键帧间隔、音频流存在，不检查 PSNR/SSIM |
| **跨方向依赖**：方向四（故障注入）需要方向一（幂等键）就位才能验证 at-least-once 的正确性 | 方向四的「验证 at-least-once」场景在阶段 0 不能运行 | 确定 | 阶段 0 的方向四只做 fail-open 审计和 `tracing::error!` 审计（不依赖方向一）。幂等键的故障注入测试在阶段 2（方向一的幂等键已部署）才加 |

### 跨团队协作建议

如果这些方向由不同的人并行实施：

| 涉众 | 方向 | 依赖 |
|------|------|------|
| 后端工程师 A | 方向一（幂等键）+ 方向四（故障注入框架） | 方向一为方向四提供验证目标 |
| 前端工程师 B | 方向二（状态同步）+ 方向三（前端限流响应） | 方向三需要方向二的状态管理基础设施 |
| QA/DevOps | 方向四阶段一（fail-open 审计）+ 方向五（媒体 E2E） | 方向五需要 Docker/ffmpeg 基础设施 |

---

## 总结

以上 5 个方向构成了一个**互相支持的非正交集合**：

- 方向一（幂等键）是系统正确性的底线，也是方向四（故障注入）的验证目标
- 方向二（前端状态一致性）和方向三（限流反馈回路）覆盖了「客户端失控」的正反两面——一个解决离线状态漂移，一个解决重连风暴
- 方向四（故障注入）提供了 CI 层面的质量护栏，让方向一的幂等键、方向二的 sync_state 协议、方向三的 429 处理都被测试覆盖
- 方向五（媒体面 E2E）是产品质量的最终验证，但出于基础设施投入的考量排在 P2

实施顺序上，我建议**从方向一和方向三开始并行**（一个后端改动、一个前端改动，不冲突），方向二紧随其后，方向四在幂等键就位后启动，方向五作为独立工作流在 CI 基础设施就绪后加入。

最关键的一点：**不要在方向二的讨论上陷入「IndexedDB vs localStorage vs Service Worker」的辩论。** 方向二的务实起点是 `sessionStorage` + `sync_state` 协议——这可以在 3 天内完成并产生可见的用户体验改善。IndexedDB 和 Service Worker 是后续的可选演进方向，不是方向二的必选项。
