现在我已完整阅读原始分析文档（`dual-write-and-consistency-gaps.md`）和审核意见。以下是我的架构分析。

---

# 架构分析报告：Aero IM 双写裂缝与状态一致性缺口

## 1. 架构评估

### 1.1 当前架构的优势

审核意见和原始文档共同确认了系统架构的几个显著优势：

**事件驱动骨架选择正确。** NATS JetStream 作为跨节点事实源 + Hub 进程内扇出的组合，在「弹性伸缩」和「本地性能」之间取得了良好平衡。这个模式的明智之处在于：它没有为每条消息走分布式共识（如 Kafka 分区 + 消费者组线性），而是用 durable consumer 的游标保证 at-least-once，用 Hub 的 bounded mpsc 控制本地扇出背压——**每个节点独立决定自己能处理多少**。

**存储层的分层有章法。** 三类存储的划分（Postgres 持久化、Redis 集群级状态、进程内存热点数据）不是随意为之的，而是体现了明确的**一致性-可用性-性能**权衡：

| 存储 | 一致性语义 | 故障影响面 |
|------|-----------|-----------|
| Postgres | 强一致（ACID 事务） | 写入失败拒绝请求（fail-closed） |
| Redis | 最终一致（TTL 自动过期） | 读取失败降级（fail-open） |
| 进程内存 | 单机强一致（RwLock/HashMap） | 进程 crash 即失 |

这种分层在正常运行时效率极高——热点数据在内存、集群视图在 Redis、权威数据在 PG。但它要求在**层间同步点**（方向一、方向五）进行审慎的设计，而这正是当前架构的薄弱环节。

**可观测性骨架已嵌入。** metrics counter 在关键路径（NATS publish 失败、消息扇出）有埋点，为诊断提供了基本抓手。但这个骨架只做到了「计数」，没做到「告警阈值 + 自动补偿」——相当于飞机有仪表盘但没自动驾驶仪。

### 1.2 架构局限性的深层诊断

审核意见识别出了 5 个方向。我将其归纳为**两类架构债务**：

#### 第一类：层间协调缺失（outbox / retry 管道）

方向一（PG↔NATS 双写）和方向四（bot 一击交付）共享同一个根本原因：**系统在「发起操作」和「确保交付」之间缺少一个持久化的协调层。** 这不是简单的「忘记加 retry」——而是架构模式层面的遗漏：

- 消息路径：在 SAGA 模式里，PG INSERT 是本地事务，NATS publish 是补偿事务。当前架构缺少了补偿事务的持久化载体（outbox 表）和执行者（outbox dispatcher）。
- Bot 路径：webhook 交付已经有完整的 retry/DLQ 管道（`delivery_sweep`），但 bot 交付没有复用这个基建。这不是技术问题，是抽象层级的缺失——「需要一个统一的出站事件交付框架，不管源是 webhook 还是 bot」。

**本质：系统缺乏一个「至少一次交付保证」的通用基础设施。** 当前的做法是通过逐条路径各自实现来拼凑这个保证——messages.rs 自己做、bot_dispatch.rs 自己做、webhook_delivery.rs 自己做。每条路径的实现质量参差不齐，且缺乏统一的监控视图。

#### 第二类：状态模型仅事件流，无快照（同步协议缺口）

方向二（WS 重连）和方向五（通话裂脑）共享另一个根本原因：**系统的状态模型是「增量事件流」，从未被设计为「快照+增量」。**

- WS 协议：`msg:*` 帧代表增量变化。重连后客户端只能从 `?since=` 回放事件流来重建状态——但事件流是**有损**的（成员变化、poll 关闭、通话结束这些事件没有完整历史）。客户端永远无法从事件流中重建出当前系统的完整视图。
- 通话状态：SfuRouter 是纯进程内存，crash 即失。没有持久化的通话拓扑快照来支持恢复。

**本质：系统缺少一个「全局状态同步协议」——不仅仅是 WS 重连恢复，而是服务端对客户端、节点对节点的完整状态视图交换机制。**

### 1.3 架构债务的累积程度

从 AGENTS.md 的 crate 地图来看，系统的模块化边界是清晰的（基础→IM→直播→组合）。但债务集中在**跨 crate 的交付保证契约**中：

| 债务项 | 影响范围 | 工时估算（参考性） |
|--------|---------|-------------------|
| 消息 outbox | `aero-im-core` + `aero-bus` + 新表 | 3-5 天开发 + 2 天测试 |
| Bot retry 管道 | `aero-server/bot_dispatch.rs` + `aero-storage` | 2-3 天 |
| WS 状态同步 | `web/` + `aero-server/ws/` + 新 API | 5-10 天（含协议设计） |
| 通话裂脑修复 | `aero-live-webrtc` + `aero-storage/call_roster.rs` | 5-8 天 |
| 乐观锁标准化 | 跨 5 个 `aero-storage/*.rs` | 3-5 天 |

**结论：不是灾难性的技术债，但方向一和方向四是必须在生产前处理的 P1 级隐患。方向二/三/五是 P2 级的产品和可靠性改善。** 这个分类与审核意见一致。

---

## 2. 扩展方向

基于审核意见的 5 个方向，我提炼出 5 个可操作的架构扩展方向。前 2 个对应审核的 P1，后 3 个对应 P2。

### 方向 A（P1）：统一出站事件交付框架

**为什么需要：**

当前系统有 4 条独立的出站事件交付路径——`publish_room_event`（消息扇出）、`bot_dispatch`（bot webhook）、`webhook_delivery`（自定义 webhook）、`push_bot`（移动推送）。每条路径有自己的重试策略、去重机制、监控埋点。这种重复造轮子的成本不仅体现在开发上，更体现为运维复杂度：一个出站事件交付失败，运维需要检查 4 个不同的位置。

**核心挑战：**

1. **抽象边界**：每条路径的语义不同——NATS publish 是「扇出给所有订阅者」，webhook POST 是「点对点投递给单个接收方」。两者需要不同的 delivery 语义（扇出 vs 点对点）。
2. **事务边界**：消息路径的 outbox 与 PG INSERT 在同一个事务中；bot/webhook 的 delivery_log 插入是独立事务。统一框架需要支持这两种模式。
3. **背压和去重**：NATS 发布的去重靠 JetStream dedup（5min 窗口）；webhook 的去重靠 delivery_id 唯一约束；bot 目前无去重。统一框架需要将这些语义暴露为配置参数。

**预期的架构变更：**

- 新增 `aero-outbox` crate（或纳入 `aero-bus`）：`OutboxRecord{id, source_type, target_type, payload, retry_policy, dedup_key, status}` 表 + `OutboxDispatcher` 后台任务（`FOR UPDATE SKIP LOCKED` + 指数退避 + dead letter）
- 消息路径：`messages.insert` 所在事务中 INSERT outbox 记录，替代当前直接 `publish_room_event`
- Bot 路径：bot_dispatch 写入 outbox，由同一 dispatcher 处理 retry
- Webhook 路径：评估是否可以逐步迁移，或保持独立（webhook 已有完整的生产级实现）

**对现有系统的影响：**

- 消息路径：**关键变更**。`send_message` 的返回路径需要变化——当前是 insert → publish → return Ok，新路径是 insert → outbox insert → return Ok（publish 变为异步）。这会影响客户端感知的延迟（更短）和错误语义（更少失败，因为 NATS 故障不会阻塞响应）。
- Bot 路径：**中低影响**。bot_dispatch 的逻辑从同步 POST 变为写入 outbox，最大的变化是 bot 事件的交付延迟从 ~100ms 变为 ~200ms（增加一次 outbox 轮询周期）。
- 不动的部分：push_bot 可以保持现有实现（推送的语义不同——系统级推送不需要重试，失败 log + metric 即可）。

### 方向 B（P1）：Bot 交付生产化

**为什么需要：**

如审核意见方向四所述，bot 交付的一次性策略**严重制约了开放平台的可信度**。企业 bot 开发者不会接受 1% 的事件丢失率。此外，空 HMAC 密钥是一个安全隐患——虽然不是立刻可利用（需要先获得 webhook URL），但在安全审计中这是一个红旗。

**核心挑战：**

1. **兼容性**：已有 bot 订阅（`bot_event_subscriptions` 表中无 `secret` 列）。增加密钥需要迁移，已有 bot 的迁移路径需要无损——旧 bot 继续用空串签名，新 bot 用密钥签名。
2. **存活探针的 UX**：URL 验证（`url_verify` 挑战-响应）会增加 Bot 创建的复杂度。需要清晰展示「验证失败」的错误信息，并允许 bot 开发者重新验证。
3. **重试策略选择**：bot 事件的语义是什么？消息事件可以重试（幂等），但按钮点击事件不能重试（用户已看到结果）。需要 bot 在注册时声明 idempotency 策略。

**预期的架构变更：**

- `aero-storage/src/bot_subscription.rs`：迁移增加 `secret TEXT NOT NULL DEFAULT ''`，创建时 `gen_random_bytes(32)` + encode as hex
- `aero-server/src/bot_dispatch.rs`：重构为写入 outbox + 使用 per-subscription secret 签名
- `aero-server/src/bot_verify.rs`：新增 `/api/bots/verify` 端点（或扩展现有创建端点），支持 URL 验证流程
- 监控：bot 交付指标（`aero-server/src/metrics.rs` 或独立模块）

**对现有系统的影响：**

- Bot 创建 API：请求体增加可选 `verification_token`，响应体增加 `secret` 字段。向后兼容——不传 verification_token 跳过验证。
- Bot 交付延迟：从同步（毫秒级）变为异步（秒级），bot 开发者的期望需要调整——如果他们需要同步响应，应该使用 REST 而非事件订阅。
- 安全影响：空串签名的旧 bot 不受影响（不强制迁移）。新 bot 获得 32 字节随机密钥。

### 方向 C（P2）：全局状态同步协议

**为什么需要：**

方向二（WS 重连）揭示的本质问题是：**当前协议是事件流的单向订阅，不是状态的双向同步。** 这不是 WS 重连的补丁问题，而是整个实时通信层需要引入的范式——让客户端和服务端共享一个「当前状态视图」的契约。

这个方向的价值不限于 WS 重连：
- **多设备同步**：用户在两台设备上，一个操作在设备 A 完成，设备 B 应能快速同步——当前的做法是等设备 B 的 WS 连接收到事件流中的增量帧。但如果设备 B 离线了一段时间，它需要全量同步一次。
- **页面刷新恢复**：SPA 中刷新页面 → 全部状态丢失 → 重新请求全部 API。全量 sync 端点可以一次调用替代 5-6 个 REST 调用。
- **服务端推状态变化**：`StateRefresh` 帧可以为后台管理操作（如管理员踢人、改名、归档）提供即时推送，而不需要客户端轮询或等待用户操作触发。

**核心挑战：**

1. **版本向量设计**：每项状态需要一个单调递增的版本号（或基于时间戳的矢量时钟）。客户端携带 `{rooms_ver: 42, polls_ver: 17, ...}`，服务端返回 `version > client_version` 的变更。设计难点在于「删除」事件——如果版本号对应的项被删除了，客户端需要知道。
2. **状态快照的大小**：一个用户可能在 200 个房间中，每个房间有未读计数、最后一条消息、角色等。完整快照可能 50-100 KB。需要增量传输和选择性加载。
3. **与服务端缓存的一致性**：当前 `participant_cache` 和 `room_member_cache` 是服务端缓存，用于加速 REST 查询。sync 端点需要决定：是读缓存（更快但可能 stale）还是读 DB（更准确但更慢）？建议：初次全量用 DB，增量用缓存。

**预期的架构变更：**

- 新端点 `POST /api/me/sync`：请求体为 `SyncRequest{rooms_ver: Option<i64>, polls_ver: Option<i64>, calls_ver: Option<i64>, presence_ver: Option<i64>}`，响应体为 `SyncResponse{rooms: Vec<RoomState>, rooms_full: bool, ...}`。如果客户端提供了版本号，只返回变更和删除的 ID 列表。
- WS 新增帧类型 `ServerFrame::StateRefresh{scope: String, payload: Value}`：服务端驱动下的状态刷新，不需要客户端请求。
- 服务端：为 rooms/polls/calls/presence 添加版本号生成器（Redis INCR 或 PG SEQUENCE）。代价低但收益高——每次状态变更时原子递增版本号。
- 客户端（`web/state.js` 或独立 `web/sync.js`）：SyncManager 模块，管理 `lastSyncVersions`，在 WS 重连后自动调用 sync 端点。

**对现有系统的影响：**

- 向后兼容：新端点不影响现有 WS 协议。`?since=` 继续工作（提供增量消息历史）。sync 端点是额外的恢复路径。
- 对现有代码的修改：**需要审计每个状态变更操作**（创建房间、改名、踢人、关闭 poll、开始/结束通话），确保它们都递增相应的版本号。这是一个横切面修改，涉及 ~30 个操作点。
- 客户端重写：`state.js` 需要扩展——当前是纯增量的状态机（收到事件就更新本地状态），需要加入「从 sync 快照初始化」的能力。这需要在状态初始化的生命周期中增加一个 sync 步骤。

### 方向 D（P2）：乐观锁标准化

**为什么需要：**

审核意见方向三的分析显示，编辑已使用乐观锁（`message.version`），但 reaction、poll、pin 等操作没有。这不只是实现不一致——它意味着核心操作没有统一的并发保证契约。

当团队成员说"系统保证消息编辑的原子性"时，他们应该说"系统保证**所有可变操作**的原子性"。当前的实现不一致意味着新开发者无法预判一个操作是否有并发保护——他需要阅读每个 `*Repo` 的实现才能知道。

**核心挑战：**

1. **Reaction 的特殊性**：Reaction 的 toggle 语义决定了它不是简单的 INSERT/UPDATE/DELETE，是"如果存在就删，如果不存在就增"。乐观锁在这个场景中不够优雅——两个并发 toggle 即使都持有正确的版本号，仍然可能产生「双边都以为点了，实际是没点」的状态。更好的做法是：**separate add/remove**（客户端发 `add_reaction` 和 `remove_reaction` 两个单独请求），由客户端控制 toggle 逻辑。
2. **Poll 计数的 read-modify-write 修正**：审核意见已指出当前实现使用 `COUNT(*)` 而非计数器，不存在 read-modify-write 问题。但 `COUNT(*)` 的模式在百万级投票时性能会差（全表扫描）。这现在是正确的，但需要关注随规模变化的性能拐点。
3. **版本冲突的 UX**：当乐观锁冲突时（`expected_version != current_version`），服务端返回 409 Conflict。客户端的重试策略——是静默重试（用户无感知），还是提示用户（请重新操作）？建议：对于秒级内的冲突，静默重试一次；超时后提示用户刷新。

**预期的架构变更：**

- 迁移：为 `message_reactions`、`poll_options`、`pins` 添加 `version INTEGER NOT NULL DEFAULT 1` 列。更新触发器自动递增。
- `aero-storage/src/reaction.rs`：转为 `add_reaction` / `remove_reaction` 双方法（替代单一的 `toggle`）。每个方法要求 `expected_version`，版本不匹配返回 `409`。
- `aero-storage/src/pin.rs`：`pin` 和 `unpin` 增加 `expected_version` 参数。注意 pin 的语义是「房间内只能有一个 pin」——多个同时 pin 的冲突需要通过版本号来拒绝。
- `aero-storage/src/poll.rs`：不需要修改 `vote`（当前实现正确），但 `close_poll` 需要版本化——两个管理员同时关闭 poll，先到者成功，后到者收到 409。
- 客户端：`ws.js` 处理 `409` 响应——对于 reaction，静默刷新本地状态；对于 poll close，提示用户「poll 已被关闭」。

**对现有系统的影响：**

- Reaction API 的语义变化最大——从 `toggle` 变为 `add/remove`。客户端需要适配：点击 reaction 时不再发 toggle，而是检查当前状态后发 add 或 remove。这增加了客户端的逻辑复杂度，但完全消除了 toggle 竞争。
- 其他操作的 API 签名变化：增加 `expected_version` 参数。对于 JS 客户端，这是一个可选参数——不传则假设 `version = 0`（无冲突保护）。向后兼容。
- 测试：乐观锁冲突的集成测试需要覆盖所有操作。建议使用 `tokio::join!` 模拟并发请求的竞态。

### 方向 E（P2）：通话状态持久化

**为什么需要：**

方向五的裂脑分析展示了通话状态的三分歧问题。通话是实时系统中对状态一致性最敏感的 feature——消息可以补历史，投票可以重新投，但通话断了几秒后**无法恢复**（ICE 状态机不会回滚）。

而且审阅意见指出了一个重要 gap：**SfuPeer 不可序列化**。这意味着「恢复通话」不是简单地从 Redis 读取状态并重建 SfuRouter——str0m 的 `SfuPeer` 是一个有状态的网络会话对象，包含 DTLS 密钥、SRTP 上下文、RTP 序列号空间和 RTCP 状态。这些不能从 Redis 重建。恢复的唯一路径是：服务端发出 re-invite → 客户端浏览器重新建立 SDP offer/answer → 创建新的 SfuPeer。

**核心挑战：**

1. **SfuPeer 不可序列化的后果**：通话恢复不能是静默的——它需要浏览器端参与。这意味着恢复协议需要是双向的：服务端发 `call_recovery{call_id, participants}` 帧 → 浏览器收到后 re-invite（新 SDP offer） → 服务端创建新 SfuPeer。整个流程需要 1-2 秒，期间浏览器显示「正在重新连接...」。
2. **Redis 写入与 NATS 信令的时序一致性**：当 Alice 加入通话时，操作的顺序是：NATS 信令（邀请）→ SfuRouter add_peer → Redis roster 更新。如果在 Redis 更新前 crash，roster 不等于 SFU 状态。需要引入事务边界——将这三个操作包装在一个「通话变更事务」中，要么全部成功，要么全部失败。
3. **心跳租约的容量**：每个 SfuPeer 每 10s 写一次 Redis TTL 续期。在 100 人通话中，每秒 10 次 Redis 写入，这不是问题。但在 1000 人直播间中（SFU 扇出），每秒 100 次写入——Redis 可以承受，但需要评估网络带宽和延迟。

**预期的架构变更：**

- `aero-storage/src/call_roster.rs`：扩展 Redis 存储结构：
  - `call:{id}:topology` → Hash `{participant_id -> node_url}`（当前 sorted set 不够——需要保存 node 关联）
  - `call:{id}:peers` → Hash `{participant_id -> peer_num}`（SfuRouter 中的 peer index，用于快速重建）
  - TTL 从 30s 延至 600s（通话最大时长），通过心跳持续续期
- `aero-live-webrtc/src/lib.rs`：新增 `recover_call(call_id, peers: Vec<ParticipantId>)` 方法，重新创建空的 SfuRouter 条目，等待 WS 帧 `call_recovery` 触发 re-invite
- `aero-server/ws/ws_impl/call.rs`：处理 `call_recovery` 帧（或扩展现有 frame 定义），将客户端发起的 re-invite 路由到 SfuRouter
- `aero-server/src/call_bridge_supervisor.rs`：在 `ensure_bridges` 循环中增加节点存活检测——如果节点不可达，对该节点上的通话发起 recovery 而不是直接断开

**对现有系统的影响：**

- 通话 API 的最小变化：新增 WS 帧类型 `call_recovery`，不改变现有 `call_*` 帧的语义。
- 服务端变化集中：SfuRouter 的创建逻辑从「收到 WS 帧 → add_peer」变为「收到 WS 帧 → add_peer **或** recover_call → re-invite」。
- 浏览器端变化：WS 连接上增加对 `call_recovery` 帧的监听，收到后重新创建 RTCPeerConnection 并发送新的 SDP offer。
- 关键决策：恢复流程中是否保留现有的 call_* 帧协议？建议保留——`call_recovery` 仅在通话拓扑恢复时发送一次，之后的 offer/answer/ICE 仍然使用现有流程。

---

## 3. 接口设计建议

### 3.1 关键接口设计原则

基于以上 5 个方向的交叉分析，我提炼出**4 条核心设计原则**：

**原则 1：所有出站交付严格分层**

```
调用方 → Outbox (持久队列) → Dispatcher (重试/背压) → Target (NATS/HTTP/Redis)
          ↑ 事务内写入        ↑ 异步后台任务
```

调用方（`send_message`、`bot_dispatch`）不应直接接触网络 I/O。它们应当写入一个持久化队列（outbox 表），由专门的 dispatcher 负责交付。这分解了三件事：
- **原子性**：调用方的事务边界清晰（只写 PG）
- **重试**：dispatcher 统一处理退避、死信、监控
- **可观测**：outbox 表本身就提供了积压视图

**原则 2：状态操作走乐观锁**

所有可变资源（reaction、poll、pin、call roster entry、user status）都应有 `version` 列。服务端方法签名：

```
fn action(&self, id: ResourceId, expected_version: i32, payload: Payload) 
    -> Result<Resource, ConflictError>
```

版本冲突返回 `409 Conflict` + 当前版本号。客户端自行决定：静默重试、提示用户刷新、或执行 merge 逻辑。

**原则 3：节点间状态交换用持久化中间存储，不用 gossip**

当前通话状态的三分歧部分来自「每个节点独立管理自己的 SfuRouter，没有全局视图」。解决方案不是引入 gossip 协议（这会增加系统复杂性），而是将通话拓扑的**权威视图**存入 Redis。

但这里需要决策：Redis 应作为「通话状态的权威来源」还是「通话状态的缓存」？建议：

- **Redis 作为权威来源**——通话创建/加入/离开时，先写 Redis（success），再更新 SfuRouter（best-effort）。读时：读 Redis，SfuRouter 作为本地性能缓存。这样状态恢复的关键路径是 Redis，不依赖其他节点。
- **折衷**：写性能（写入 Redis 比写入内存慢 ~10ms）和一致性（Redis 故障意味着通话管理不可用）。对于通话这个场景，10ms 的代价是可接受的。

**原则 4：客户端状态同步是「快照 + 增量」，仅增量**

`POST /api/me/sync` 提供全量快照，WS 协议继续提供增量事件流。客户端在以下三种场景使用快照：
1. 首次加载（页面初始化）
2. 重连后（WS 恢复连接）
3. 周期性刷新（每 30 分钟）

增量事件流用于实时状态更新。快照是恢复机制，不是主数据通路。

### 3.2 是否需要新的抽象层

**需要引入一个抽象层：`aero-outbox` crate（或 `aero-delivery`）。**

当前缺少一个「至少一次交付保证」的统一框架。4 条出站路径各自实现了类似的逻辑，但细节不同：

| 组件 | 持久化 | 重试 | 去重 | 死信 |
|------|--------|------|------|------|
| `publish_room_event` | ❌ | ❌ | ✅（JetStream dedup） | ❌ |
| `bot_dispatch` | ❌ | ❌ | ❌ | ❌ |
| `webhook_delivery` | ✅（delivery_log 表） | ✅（指数退避） | ✅（delivery_id） | ✅ |
| `push_bot` | ❌ | ❌ | ❌ | ❌ |

引入 `Outbox` 抽象层后：

```rust
// 统一出站交付 trait（示意，非实现）
trait OutboxDispatcher {
    /// 写入出站记录（调用方在事务内调用）
    async fn enqueue(&self, tx: &mut Transaction, record: OutboxRecord) -> Result<()>;
    
    /// 后台轮询循环（独立 tokio task）
    async fn run(self, shutdown: CancellationToken);
    
    /// 死信队列的 requeue
    async fn requeue(&self, record_id: Uuid) -> Result<()>;
}
```

但这不意味着「所有出站路径都用同一张 outbox 表」。消息的 outbox 可以单独用 `outbox_messages` 表（与 `messages` 在同一个事务中），bot 和 webhook 的 outbox 可以共享 `delivery_log` 表（或扩展它）。关键在于**抽象层统一了行为契约**（重试、退避、死信、监控），但允许不同物理表。

### 3.3 向后兼容性策略

每个方向的向后兼容策略：

| 方向 | 兼容性影响 | 策略 |
|------|-----------|------|
| **Outbox（消息）** | `send_message` 响应更快（异步化），但返回语义不变（`Ok(Message)`） | ✅ 完全向后兼容——行为变化是性能改善，不是 API 变化 |
| **Bot 交付生产化** | `POST /api/bots` 新增可选字段 `verification_token`；响应新增 `secret` | ✅ 兼容——不传验证 token 则跳过验证（空串密钥）。已有 bot 不受影响 |
| **状态同步协议** | 新增 `POST /api/me/sync`，不影响现有 `?since=` | ✅ 全新端点，零影响 |
| **乐观锁标准化** | `add_reaction`/`remove_reaction` 替代 `toggle`；所有 PUT/PATCH 接受 `expected_version` | ⚠️ **需要客户端适配**——reaction 是主要变化。建议：服务端保留 `toggle` 作为别名（内部 fallback 到 `add` + 无并发保护），给客户端 2 个版本的过渡期 |
| **通话持久化** | 新增 `call_recovery` WS 帧 | ✅ 不修改现有帧——新帧可选处理。旧客户端忽略即可 |

乐观锁标准化是**唯一需要客户端代码修改的方向**（reaction 从 toggle 变为 add/remove）。建议的实施路径：

```
版本 N：服务端新增 add_reaction/remove_reaction + 保留 toggle（作为 add 的别名，无并发保护）
版本 N+1：web 客户端切换到 add/remove
版本 N+2：移除 toggle（服务端删除该端点）
```

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

**不需要引入全新的中间件或存储系统。** 当前技术栈（Postgres + Redis + NATS + Rust/axum）足够支撑所有 5 个方向的扩展。理由：

| 方向 | 可用设施 | 为什么够 |
|------|---------|---------|
| Outbox（消息） | Postgres 表 + `FOR UPDATE SKIP LOCKED` | 现有 `ai_jobs` 表已经用相同的模式实现后台工作者——方案已被验证 |
| Bot retry | Postgres delivery_log 表 | 现有 `webhook_delivery.rs` 已经实现——复用 |
| 状态同步 | REST + WS | 当前 API 层已足够承载新端点和新帧类型 |
| 乐观锁 | Postgres 列 + `UPDATE ... WHERE version = $N` | 迁移 0157 已实现了消息编辑的乐观锁——标准化即可 |
| 通话持久化 | Redis Hash + TTL | 现有 `CallRosterStore` 已使用 Redis——扩展结构即可 |

**唯一的潜在引入：分布式追踪（OpenTelemetry tracing）**。当前系统的可观测性是 metrics 为主的（Prometheus counters）。对于 outbox 这种异步交付路径，metrics 只能告诉你「有没有积压」，不能告诉你「这条消息的完整交付链路花了多久」。建议：

```
消息 send → outbox enqueue → outbox dispatch → NATS publish → bus_listener → Hub fan_out → WS deliver
                                                                    ↑
                                这个链路跨进程，metrics 不够，需要 tracing
```

但这是一个 **"nice to have"** 而非阻塞项——在实施方向一（outbox）时，可以先通过 `outbox_messages` 表的状态列（`pending → delivering → delivered → dead`）来观察积压。tracing 可以后续添加。

### 4.2 第三方依赖的评估标准

对于新 crate 或模块的依赖选择，建议使用以下评估矩阵：

| 维度 | 权重 | 说明 |
|------|------|------|
| **MSRV 兼容** | 阻断 | 必须 ≤1.80（当前 MSRV） |
| **Rust 社区活跃度** | 高 | GitHub stars > 500，最近 6 个月有提交 |
| **审计友好（unsafe 用量）** | 高 | `unsafe_code = "forbid"` 是根 crate 的硬性 lints |
| **tokio 生态兼容** | 高 | 必须支持 tokio（当前异步运行时） |
| **许可证兼容** | 中 | 必须 MIT/Apache-2.0 双许可或类似宽松许可 |

**对于方向 E（通话持久化），需要注意：** `str0m` 的 `SfuPeer` 不可序列化是设计选择（DTLS 状态机是内存敏感的），不是依赖限制。通话恢复必须通过重建 session（re-invite），而非依赖序列化/反序列化。这不是「找个更好的 webrtc 库」能解决的问题——所有纯 Rust WebRTC 库（str0m、webrtc-rs）都有类似的内存状态约束。

### 4.3 自建 vs 采购的决策

当前 5 个方向都涉及「自建」能力——不涉及采购第三方服务。但有两条决策边界需要明确：

**边界一：出站事件框架——自建 vs 用消息队列的预置能力**

| 选择 | 优点 | 缺点 |
|------|------|------|
| **自建出站 outbox（基于 PG）** | 与消息 INSERT 同一事务；无需引入新基础设施；已有 `ai_jobs` 可参考 | 轮询模式延迟 ~100ms；PG 负载增加 |
| **用 NATS JetStream 作为 outbox** | 延迟更低（pub/sub 异步）；JetStream 已有重投和去重 | 消息 INSERT 和 outbox 存入不在同一事务中——失去了方向一的核心目标（事务原子性）；NATS 故障时 outbox 不可用 |

**结论：自建 PG outbox**。事务原子性是方向一的核心目标，不能为了技术统一而牺牲。NATS 可以作为 outbox 的目标端，但 outbox 本身（持久化队列）必须在 PG 中。

**边界二：通话拓扑——Redis vs 专用状态协调组件（etcd/consul）**

| 选择 | 优点 | 缺点 |
|------|------|------|
| **Redis Hash + TTL** | 现有基础设施；无需新运维依赖；延迟 <1ms | TTL 过期有模糊性（节点故障 vs 心跳延迟）；无 watch 机制（需要轮询） |
| **etcd** | 强一致（Raft）；watch 机制（实时通知）；lease 机制（精确租约） | 新基础设施引入；Go 生态与 Rust 集成成本；运维复杂度增加；社区 vs etcd-rs 成熟度 |

**结论：先用 Redis**。等通话成为系统中一个经过验证的流量场景后再考虑 etcd。Redis 的 TTL 模糊性可以通过「TTL 设置为心跳间隔的 3 倍」来缓解（心跳 10s，TTL 30s，允许 2 次丢失）。

---

## 5. 实施路线图

### 5.1 优先级排序

我认同审核意见的 P1/P2 分类，并根据「生产阻塞程度」和「对既有系统的改动量」做微调：

| 优先级 | 方向 | 为什么在这个层级 |
|--------|------|----------------|
| **P0** | — | 当前无 P0——系统可运行，没有「挂了」的级故障 |
| **P1** | 方向 A：统一出站事件交付框架（含消息 outbox + bot retry） | 消息神隐 + bot 事件丢失 = 不可用于生产。这是企业级 IM 的最低门槛 |
| **P1** | 方向 B：Bot 交付生产化 | 与方向 A 紧密耦合——bot retry 管道可以复用方向 A 的 outbox 框架。建议一起做 |
| **P2** | 方向 C：全局状态同步协议 | 高用户可见度，但系统可以在没有它的情况下工作——用户需要手动刷新页面作为替代 |
| **P2** | 方向 D：乐观锁标准化 | 投票和 reaction 的并发问题有低概率发生（活跃大群才有），且影响面有限（用户可再次操作）。代码层面的标准化是模块健康度的投资，不是功能阻塞 |
| **P2** | 方向 E：通话状态持久化 | 通话功能尚未经过大规模生产验证。在通话变为核心场景或进入多节点部署后再实施 |

### 5.2 阶段划分

**阶段一（P1 · 4-6 周）：出站交付标准化 + Bot 管道**

```
第 1-2 周：设计 + 迁移
  - Outbox 表设计（消息 + bot 共享 delivery 抽象层，物理分离或统一？）
  - 评估：消息 outbox 独立表（outbox_messages）、bot 复用 delivery_log 扩展
  - 迁移：outbox_messages 表 + bot_event_subscriptions.secret 列

第 2-4 周：实现
  - aero-outbox crate：OutboxDispatcher + run() 循环（FOR UPDATE SKIP LOCKED + 指数退避）
  - 消息路径：send_message 改为写入 outbox → 返回 Ok；dispatcher 负责 NATS publish
  - Bot 路径：bot_dispatch 改为写入 outbox → dispatcher 负责 HTTP POST + 重试
  - 存活探针：/api/bots/verify 端点

第 4-6 周：测试 + 监控
  - 集成测试：NATS 断开时消息仍能持久化，恢复后自动投递
  - 集成测试：bot 503 返回后重试 5 次后进入 dead 状态
  - 监控：outbox 积压 gauge、delivery_latency_histogram、dead_letter_counter
  - 文档：操作手册更新（如何查看 outbox 积压、如何 requeue dead bot 事件）

里程碑：消息交付无丢失 + bot 事件 3 次重试 + 存活探针
```

**阶段二（P2 · 4-8 周，与阶段一可并行）：状态同步 + 乐观锁**

```
第 1-2 周：协议设计
  - SyncRequest/SyncResponse 格式设计
  - 版本号生成策略（Redis INCR vs PG SEQUENCE vs 墙上时钟）
  - 增量 vs 全量的边界（版本差超过 X 则返回全量）

第 2-4 周：服务端实现
  - POST /api/me/sync 端点（全量版本先做，增量版本后做）
  - 各操作（房间创建/改名/踢人、poll 开/关、通话开始/结束）的版本号递增埋点 + 可测试性设计
  - 乐观锁：reaction（add/remove）、pin（expected_version）、close_poll（expected_version）

第 4-6 周：客户端实现（可延迟到 JS 团队接手）
  - web/sync.js：SyncManager 模块
  - ws.js：重连后先 sync 再 joinRoom的 控制流
  - state.js：支持从 sync 快照初始化

第 6-8 周：集成 + 端到端测试
  - 模拟 100 个并发用户重连，验证状态一致性
  - 乐观锁并发测试：tokio::join! 模拟同时 reaction/poll/pin

里程碑：WS 重连后房间列表/poll 状态/通话状态自动恢复；reaction/pin 原子性保证
```

**阶段三（P2 · 4-6 周，依赖阶段二完成后）：通话持久化**

```
第 1-2 周：Redis 存储结构重设计
  - call:{id}:topology (Hash)
  - call:{id}:state (String)
  - 现有的 CallRosterStore sorted set 如何演化（弃用 vs 共存）

第 2-4 周：实现
  - SfuRouter.recover_call() 方法
  - call_bridge_supervisor 节点存活检测 + 触发恢复流程
  - WS 帧 call_recovery 处理
  - 心跳写入（每个 SfuPeer 每 10s 续期 Redis TTL）

第 4-6 周：测试
  - 模拟节点 crash → 新节点启动 → 通话恢复
  - 模拟网络分区 → 恢复后 roster 一致性校验
  - 心跳超时 → 自动清理 ghost peer

里程碑：节点故障后通话能在 5 秒内恢复
```

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| **Outbox 实现引入新的双写**（PG outbox + NATS） | 中 | 高 | 注意：outbox 本身是一次写入（PG），NATS 是读取后的目标。不存在双写。但 outbox 表的管理（定期清理已投递记录）需要 `pg_cron` 或定时 job |
| **乐观锁冲突率在大型群聊中过高**（reaction 并发） | 高 | 中 | 监控 `409 Count`。如果每千次 reaction 中 >5 次冲突，考虑 reaction 的最终一致性——放弃乐观锁，使用幂等插入 + 客户端侧去重 |
| **通话恢复的浏览器端 re-invite 延迟** | 中 | 中 | 预建立 ICE 候选池。在正常通话中持续收集 ICE 候选，recovery 时跳过收集阶段，直接发送 offer |
| **POST /api/me/sync 负载过高**（每次重连都调用） | 中 | 中 | 使用 HTTP 缓存（`ETag` 头 + `If-None-Match`）。sync 端点的响应可缓存——如果版本号无变化，返回 `304 Not Modified` |
| **Redis 成为通话管理的单点故障** | 低 | 高 | Redis Sentinel 或 Redis Cluster。当前系统已经有 Redis 依赖，这不是新增风险。通话管理应该像房间管理一样处理 Redis 故障——降级为单节点模式（通话只在同一节点内） |
| **多个方向并行实施导致相互阻塞** | 中 | 中 | 方向 A/B 与方向 C/E 的改动集不相交（A/B 影响 `aero-im-core` + `aero-bus`，C/E 影响 `aero-server/ws/` + `web/`）。可通过 git worktree 分开实施 |

---

## 结语

审核意见的分析是准确的，5 个方向的论证充分、代码证据扎实。方向三（Poll）的修正建议已采纳在本文档中。方向五（通话）的 SfuPeer 不可序列化 gap 已补充。

**建议立即启动的方向：方向 A + 方向 B（统一出站交付框架 + Bot 生产化）。** 这是系统从"可以跑"到"可以用于生产"的关键一跃。其他方向可以有序排入后续迭代。

核心的架构洞见是：当前系统的**交付保证不是 across-the-board 的——消息路径没有保证、bot 路径没有保证、webhook 路径有保证**。这不是资源不足的体现，而是缺乏一个统一的「至少一次交付」抽象层的结果。一旦这个抽象层建立（方向 A），消息、bot、webhook 三条路径将获得一致的可靠性保证，且运维复杂度从 O(n) 降低为 O(1)。这才是架构改进的真正 ROI。
