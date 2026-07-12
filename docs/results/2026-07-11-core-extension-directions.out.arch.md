好，我已经完整阅读了分析文档 `docs/requirements/2026-07-12-core-expansion-direction-analysis.md`，并结合了项目已有的设计 Spec（`2026-05-22-aero-im-design.md`）和 AGENTS.md 上下文。下面从架构师视角进行系统分析。

---

# 架构分析：Aero IM 核心扩展方向

## 1. 架构评估

### 1.1 核心优势

**事件驱动骨架选择正确**。NATS JetStream 作为跨实例事实源 + per-process `Hub` 做本地 `mpsc` 扇出，在 Rust 所有权模型下是一个非常干净的组合：

- 单进程扇出的 `mpsc` 避免了跨进程一致性问题——`bind` 到本地 `Hub` 的连接天然是进程内资源，`fan_out_raw` 的 bounded channel 在 `try_send` 失败时 `lossy` 或 `disconnect` 是本地决策，不需要跨节点协调。
- Durable consumer (`aero-server`) + Ephemeral consumer (`live.stream.*`) 的使用场景区分精准——IM 消息需要 at-least-once 保证，弹幕可以接受偶尔丢几条。
- **SeqGate + per-subject 单调 seq** 已经在客户端做了去重基础，为后续的 delivery cursor 和重连回填提供了天花板。

**Crate 分层克制且可测试**。16 个 crate 从 `aero-common`（纯叶子类型）到 `aero-server`（组合层），依赖方向清晰无环。`aero-common` 中的 `Block`/`RoomEvent`/`StreamEvent` 等共享类型在 15 个 crate 间流通但不形成循环依赖——这是 Rust workspace 容易踩坑的地方，设计团队做得对。

**迁徙式交付节奏成熟**。157 个迁移（migrations）说明团队对 schema 演进的纪律性强。配合编译期嵌入 migration（`sqlx::migrate!("../../migrations")`）的约束，避免了 schema drift。

### 1.2 结构性局限

**（1）O(N) 消息扇出在超大房间面临二次曲线拐点**

这是最根本的架构局限，也是分析文档方向一的核心发现。当前架构的写路径复杂度是：

```
send_message → O(成员数) for:
  1. RoomMemberCache 展开
  2. Hub::fan_out_raw 遍历本地连接
  3. dispatch_notifications 成员级判断
  4. push_bot 推送
```

在 100 人房间里这个成本可忽略；在 10 万人房间里，每次 `@everyone` 的 push 成本是 100k 条 FCM 请求 + 100k 条 `notification` 行。**这里有一个重要的架构决策点：当前系统是设计为 Slack（~5000 人/频道）还是 Discord（~25 万人/频道）——这决定是否需要写路径的分层降级策略。**

**（2）WebSocket 协议层与持久化游标之间有一条「已修路未通车」的 gap**

分析文档方向三精准指出了问题。`DeliveryCursorRepo` 已建（0153 迁移），`?cursors=1` 参数已定义，但 `ws.js` 传 `since`（全局单游标），服务端 `parse_resume_cursor` 也只用 `backfill_since`。这一层基础设施完备但最后一百米未通的模式值得警惕——这表明团队在协议演进时，backend-first 的工作流可能导致 WS 协议层成为整合瓶颈。

**（3）审计日志与主业务表的写路径耦合**

`soft_delete_audited` 在同一事务中写入主表和审计表。在写放大场景下，审计日志的写入可能成为主链路的阻塞点。这不是当前问题，但如果在方向一（写放大控制）中做懒加载扇出或批量删除，审计路径需要重新审视其事务边界。

**（4）drain 与 cancel 的区别未严格定义**

AGENTS.md 提到 `CancellationToken` 优雅关停，但某些后台定时器（`blob_gc_drain`、`observability_gauge_samplers`）明确标注「不持 cancel token」。这些 timer 在进程关停时的行为是可预期的（随进程 die），但在 K8s 环境下，`SIGTERM` 后的 graceful shutdown window（默认 30s）内，未接入 cancel token 的 timer 可能在 drain 过程中提交半截操作。

### 1.3 架构债务

| 债务类型 | 位置 | 风险等级 | 说明 |
|---|---|---|---|
| 协议层不一致 | `delivery_cursor` 已建但 WS 未接入 | 高 | 重连消息丢失的生产事故风险 |
| 多设备状态空白 | 跨标签 WS 独立，无状态同步 | 中 | 用户感知不一致 |
| Web 前端架构债 | 裸 ES + 全局 mutable state + 无测试 | 高 | 阻止产品化发布 |
| `@everyone` 无上游防护 | `dispatch_notifications` 无冷却/限流 | 高 | 运营安全风险 |
| 弹幕媒体时间戳缺失 | `StreamChatLine` 只有 wall-clock | 中 | 直播体验缺陷 |
| 部分 timer 无 cancel token | `blob_gc_drain` 等 | 低 | K8s 关停竞争 |
| 审计写路径耦合 | `soft_delete_audited` 同一事务 | 低 | 未来扩展可能受限 |

---

## 2. 扩展方向

### 方向 A（优先级 P0）：重连 TOCTOU 消除 + Delivery Cursor 全线贯通

> 对应分析文档方向三，这是面投成本最低但收益最高的修复。

**为什么需要**：消息可靠性是 IM 产品的信任基线。在 at-least-once 语义下，重连丢失消息是不可接受的。当前 `since=` 全局单游标在以下场景必然失败：多设备断连后重新同步、新加入房间后的首次重连、以及跨房间时间线交织时的间隙损失。

**核心挑战**：

1. **多设备游标合并**：`DeliveryCursorRepo::advance` 的 `seq > stored_seq` 单调合并机制在同一 participant 多设备同时写时存在信息丢失。假设设备 A 在房间 X 的最新 seq=100，设备 B 在房间 Y 的最新 seq=120，且 X 和 Y 是两个不同房间。当 A 和 B 同时（或几乎同时）写游标时，`max(100, 120)=120` 被持久化。但如果房间 Y 只在设备 B 上有 membership（A 不在 Y 里），Y 的 seq=120 对 A 的 next reconnect 路径无意义；而房间 X 的 seq=100 被 120 覆盖后，A 不会丢失消息（因为 `backfill_since` 用的是创建时间戳，不是 seq），但游标的语义从「我在各房间看到的最后一条消息」退化为「全局最大值」。

2. **游标回填与 live 扇出的重复投递**：这是 prompt 中提到的关键问题。当 delivery cursor 接入后，连接恢复流程变为：
   - 升级 WS，传 `cursors`（per-room 游标）
   - 服务端逐个房间执行 `backfill_cursor(RoomId, Seq)` → 返回未送达消息
   - 同时 `run_bus_listener` 可能正在向该 participant 扇出新消息
   - 双路径可能投递同一条消息

   解法已在客户端验证（`SeqGate`），但需要确认 `backfill` 路径的 seq 是否与 live 路径使用同一命名空间。如果 backfill 给的消息也带了 `seq` 字段，SeqGate 能正确去重；如果 backfill 消息上游标有偏移，需要 `SeqGate` 的初始化阈值正确。

3. **`cursors` 缺失时的退化行为**：如果 `DeliveryCursorRepo` 中没有该 (participant, room) 行（首次连接或数据清除），`advance` 返回默认值。这意味着回填全部历史——对于 10 万条消息的频道，需要在 WS 握手过程中完成全量同步。需要 `max_catchup_messages` 硬限制 + 分页回填。

**预期架构变更**：

```
修改范围：
├── web/ws.js
│   ├── constructor: 初始化 per-room cursors Map (取代 this._lastSeen)
│   ├── connect(): cursors=1 改为 serialized per-room seq map
│   └── on_message: 每个消息更新对应 room 的游标
├── crates/aero-server/src/ws/ws_impl/mod.rs
│   ├── parse_resume_cursor: 扩展支持 cursor map 解析
│   ├── backfill_room_ids: 改为 backfill_cursors
│   └── 新加: backfill → SeqGate 桥梁(确保 seq 命名空间统一)
├── crates/aero-storage/src/delivery_cursor.rs
│   ├── advance: 批量 upsert (vs 当前单行)
│   └── 新加: batch_fetch(participant, rooms) → Vec<Cursor>
└── 新加: bus/seq.rs 中 per-room seq 的 backfill 查询接口
```

**对现有系统影响**：
- 低侵入。不涉及 NATS 消息格式变更，不涉及 Hub 扇出路径修改。
- 客户端改动集中（`ws.js` + 本地存储），服务端改动收敛（`mod.rs` 的 resume 逻辑 + `delivery_cursor.rs` 的批量接口）。
- 迁移：不需要新迁移（复用 0153）。
- 向后兼容：如果客户端不传 `cursors`，fallback 到当前 `since=` 逻辑。

**多选项分析**：

| 选项 | 客户端存储 | 服务端变更 | 复杂度 | 消息可靠性 |
|---|---|---|---|---|
| A. 仅修复 `since` → per-room cursors 走内存 | 页面刷新丢失 | 低 | 低 | 中（不跨 session） |
| B. 内存 + sessionStorage 持久化 cursors | sessionStorage | 低 | 低 | 中（单 tab） |
| C. 内存 + IndexedDB 持久化 cursors | IndexedDB | 低 | 中 | 高（跨 tab 跨 session） |
| D. 服务端全权管理游标（当前 0153 方案） | 无 | 中 | 低 | 最高（设备无关） |

**建议选 D**（当前 migration 已建，补完最后一百米即可），配合客户端 IndexedDB 做增量缓存以降低回填开销。

---

### 方向 B（优先级 P0）：@everyone 通知风暴上游防护

> 对应分析文档方向四 + prompt 中关于复用 `rate_limiter` 的建议。

**为什么需要**：这是运营安全风险，不是功能需求。一个被 @everyone 刷屏的工作区会触发 100k 级别的通知生成、DB 写入、推送请求。在用户信任和推送成本两个维度上都需要防护。AGENTS.md §4.1 只约束了 `broadcast` API（`targets.len() <= 100`），但 `@everyone` 绕过了该路径。

**核心挑战**：

1. **冷却粒度的选择**：per-room 冷却（房间 A 的 @everyone 不影响房间 B）vs per-sender 冷却（一个人 5 个房间同时 @everyone 是否独立计数？）vs per-workspace 冷却（工作区维度限流）。建议三层校验（由粗到细）：
   - 全局：`AERO_EVERYONE_GLOBAL_COOLDOWN_SECS`（默认 10s，每个房间独立）
   - per-room：`AERO_EVERYONE_ROOM_COOLDOWN_SECS`（默认 60s）
   - per-sender + per-room：`AERO_EVERYONE_SENDER_COOLDOWN_SECS`（默认 300s）
   
   优先级：per-sender+room 最精准但 map 增长可能成为 DoS 放大器（需要 idle sweep 配合）。

2. **`@here` 的降级风险**：分析文档指出，`here_recipients` 在 Redis 故障时 fail-open 到全成员列表。此时的 `@here` 等价于 `@everyone`。如果没有告警标记此类降级，运营人员不知道一个「只给在线成员」的消息实际上变成了全员广播。

3. **`@everyone` 的语义细化**：大型工作区可能需要区分 `@everyone`（全员）和 `@channel`（仅频道成员——但大型频道全员也是 10 万）。可以考虑引入广播角色（BroadcastRole）：
   - `BroadcastRole::AdminOnly`：只有工作区 Owner/Admin 可以 @everyone
   - `BroadcastRole::RoleBased`：特定角色可以
   - `BroadcastRole::Cooldown`：任何人可以，但有冷却

4. **与 `post_policy`（migration 0030）的协同**：`post_policy` 已经限制了谁能发消息。被授权的人可以用 @everyone。防护应叠加在 `post_policy` 之上，不取代它。

**预期架构变更**：

```
修改范围：
├── crates/aero-im-core/src/service/orig.rs
│   ├── dispatch_notifications: 新加 @everyone 速率门控
│   ├── 新加: check_everyone_rate(room, sender) → Result
│   └── here_recipients: 新加降级告警 metrics 计数器
├── crates/aero-storage/ 或 crates/aero-common/
│   ├── 新加: EveryoneRateLimitConfig（冷却时间阈值）
│   └── 或在已有 RateLimitConfig 中扩展字段
├── crates/aero-server/src/routes/
│   ├── 新加 metrics: aero_everyone_rate_limited_total
│   └── 新加: aero_here_degraded_to_everyone_total（降级告警）
└── 无需新迁移（复用 per-client token bucket 机制或 Redis sorted-set）
```

**复用什么**：AGENTS.md §4.1 提到已有 `rate_limiter` 基础设施和 `spam-guard` 空闲 sender 清扫。可以直接复用 `RateLimitSweeper` 做 per-room `@everyone` bucket 的空闲驱逐，不需要新引入依赖。

**对现有系统影响**：
- 极低。不改变消息格式，不改变扇出路径，只在上游加一道门。
- `@everyone` 被限流时返回 429（或 WS 帧中的 `rate_limited` 错误码），消息本身正常存储但通知路径被阻止。这是一个设计选择：是先拒消息再拒通知，还是存消息但静默抑制通知？建议后者（消息正常落库，通知被节流），因为用户写了消息不应该被 429 丢失内容，只是通知风暴被抑制。

---

### 方向 C（优先级 P1）：写放大控制的懒惰扇出 + 在线优先策略

> 对应分析文档方向一，这是根本性的架构能力提升而非 bug 修复。

**为什么需要**：方向 B 的 `@everyone` 限流只是治标。当房间达到 10 万数量级时，即使是普通消息（@ 了一个人），`RoomMemberCache` 的 O(N) 展开也是浪费——99.9% 的成员根本不关心这条消息，他们只在下次打开频道时通过 `backfill` 看到新消息。**核心洞察：消息扇出的上限不应该与房间成员数绑定，而应与同时在线观看该房间的连接数绑定**。

**核心挑战**：

1. **Hub 在线名单与房间成员名册的分离**：当前 `handle_room_event_sub` 展开房间全成员列表，然后用 `online_members` 做交集。在 10 万人房间里，全成员列表展开本身是 O(N)。需要改为：对非 `@everyone` 的消息，只展开 `online_members` + `explicit_recipients`（@提及的人）+ 线程订阅者。离线成员完全跳过。

2. **`NotifyBatch` 的分段编码**：当前 `NotifyBatch` 在一个 NATS 消息中包含全部收件人（约 6MB JSON 在 10 万人场景）。分段策略：如果 `NotifyBatch` 收件人超过阈值（如 1000），拆分为多个 NATS 消息，每个 Hub 实例只处理与自己本地连接子集相关的片段。这需要 `NotifyBatch` 带 `hub_id` 路由 hint。

3. **通知路径的条件跳过**：纯文本消息（无 @、无回复、无关键词匹配）对离线成员完全不需要通知。当前 `dispatch_notifications` 在 DB batch 查询前已经展开了全成员列表。优化点：先把收件人集合缩小到「可能需要通知」的子集（在线成员 + @mentioned + 关键词匹配 + 线程订阅者），再去做 DB 查询。

**预期架构变更**（这是一个中期工程，建议分两阶段）：

```
Phase 1（轻度优化）：
├── RoomMemberCache: 增加在线成员过滤缓存（缓存层，非 Redis 往返）
├── handle_room_event_sub: 优先用 online_members，回退全列表
├── NotifyBatch: 增加分段发送（>1000 收件人拆分）
└── dispatch_notifications: 先展开收件人子集，再 batch 查询

Phase 2（深度优化）：
├── 新 trait: FanOutStrategy（OnlineOnly | AllMembers | ExplicitRecipients）
├── Hub::fan_out_raw: 改为接受 FanOutStrategy，只扇出在线连接
├── 新: OfflineBackfillQueue（离线成员的消息通过 backfill 投递）
└── 新迁移: 无（全部改造在代码层）
```

**对现有系统影响**：
- Phase 1 可以渐进式部署，每个改动独立验证。
- Phase 2 的 `FanOutStrategy` 需要修改 `Hub` 的接口签名，影响 `run_bus_listener` 和 `run_live_bus_listener`。建议通过 trait 对象 + 默认实现（当前行为）来保持向后兼容。

**不推荐的策略**：不要引入「消息分片」或「房间分片」。当前每个房间在一个 NATS subject 上，分片会引入跨分区排序和 SeqGate 的复杂性，收益有限。

---

### 方向 D（优先级 P1）：弹幕-媒体时间轴一致性与录播回放同步

> 对应分析文档方向五 + prompt 中 PCR 回绕的提醒。

**为什么需要**：当前直播弹幕和 HLS 视频流是两个独立通道。弹幕的 `server_timestamp` 是 wall-clock（服务端收到时间），与 HLS 播放器的时间轴（基于 PCR/PTS）没有映射关系。导致的用户体验问题：弹幕在真实事件时间出现（如果有 HLS 10s 延迟，弹幕比对应视频帧早 10s 出现）；录播回放时弹幕完全无法同步。

**核心挑战**：

1. **媒体时间戳的捕获点选择**：有三种可能的捕获点：
   - **方案 A**：从 WHIP/RTMP/SRT 推流端获取 RTP timestamp（最精确，但不同摄入协议格式不同，WHIP 有 RTP，RTMP 无 RTP 只有 FLV timestamp，SRT 有 TS 但不暴露 RTP）
   - **方案 B**：从 HLS 切片器（`FlvToTsConverter`）获取写入的 PCR/PTS 偏移，反向推算消息对应的分片位置（通过 wall-clock 到 PCR 的映射关系）
   - **方案 C**：在客户端（hls.js）通过 `HTMLVideoElement` 的 `currentTime` 与弹幕的 wall-clock 做相对偏移，不依赖服务端

2. **PCR 回绕（prompt 指出的 26.5h 问题）**：MPEG-TS PCR 是 33-bit 27MHz 时钟，约 26.5 小时回绕一次。如果一场直播持续超过 24h（如 24/7 直播频道），PCR 回绕后，**新推流的 PCR 值小于旧值**。存储为单一 `media_timestamp: i64` 的方案会出问题——回填时弹幕定位会落在视频开头。

   建议的存储表示：
   ```
   ┌──────────────────────────────────────────────┐
   │ StreamChatLine 扩展字段                       │
   │ ├── media_timestamp: Option<i64>             │
   │ │   → 单调递增的 90kHz 时钟计数（相对于推流开始）│
   │ ├── media_base: Option<DateTime<Utc>>         │
   │ │   → 推流开始时刻（wall-clock 参考点）         │
   │ └── media_epoch: Option<i32>                 │
   │     → PCR 回绕次数（0, 1, 2...）               │
   └──────────────────────────────────────────────┘
   ```
   
   这样 `effective_timestamp = media_epoch * 2^33 + media_timestamp` 可以覆盖任意时长的直播。

3. **不同摄入协议的统一抽象**：WHIP（RTP timestamp）、RTMP（FLV timestamp、无 RTP）、SRT（MPEG-TS PCR 但可能已在 demux 中丢弃）。需要一个 `MediaTimestampProvider` trait，每种摄入实现一个 adapter：

   ```rust
   trait MediaTimestampProvider {
       /// 推流开始时建立参考点
       fn init(&mut self, base_wall_clock: DateTime<Utc>);
       /// 返回相对于推流开始的单调递增 90kHz tick
       fn current_tick(&self) -> Option<(i64, u32)>; // (tick, epoch)
       /// PCR 回绕后的 epoch 计数器
       fn epoch(&self) -> i32;
   }
   ```

   这对不同协议的实现难度：WHIP（trivial，RTP timestamp 就是 90kHz）+ RTMP（需从 FLV timestamp 推算，32-bit ms ~49.7 天回绕）+ SRT（需要从 TS demux 中保留 PCR，难度最大）。

**预期架构变更**：

```
修改范围：
├── crates/aero-live-core/src/lib.rs
│   ├── StreamChatLine: 增加 media_timestamp / media_epoch 字段
│   ├── StreamGiftLine: 同上
│   ├── 新 trait: MediaTimestampProvider
│   └── 新 struct: MonotonicMediaClock（处理 PCR 回绕）
├── crates/aero-live-whip/src/session.rs
│   ├── 实现 MediaTimestampProvider for WhipSession
│   └── on_rtp: 把 RTP timestamp 挂到 chat line
├── crates/aero-live-rtmp/src/
│   ├── 实现 MediaTimestampProvider for RtmpSession
│   └── FLV timestamp → 90kHz tick 转换
├── crates/aero-live-srt/src/
│   ├── 实现 MediaTimestampProvider for SrtSession
│   └── 保留 TS PCR 信息
├── crates/aero-server/src/live.rs
│   ├── LiveService::post_chat: 接受 media_timestamp 参数
│   └── LiveService::post_gift: 同上
├── web/live.js
│   ├── 新: MediaTimelineSync（监听 hls.js 播放时间）
│   └── on_message: 在 media_timestamp 匹配时显示弹幕
├── web/hls-player.js（或 app.js 的 HLS 部分）
│   ├── 暴露 currentMediaTime（基于 hls.js 的 liveSyncPosition）
│   └── 注册回调: onMediaTimeChange(cb)
└── 新迁移: NNNN_stream_chat_add_media_timestamp.sql
    ├── ALTER TABLE stream_chat ADD COLUMN media_tick bigint
    ├── ADD COLUMN media_epoch integer
    └── ADD COLUMN media_base timestamptz
```

**对现有系统影响**：
- 新字段为 `Option`，向后兼容。旧弹幕无媒体时间戳，replay 按 wall-clock 显示（降级行为）。
- 需要修改 `post_chat`/`post_gift` 的路由签名（增加 `media_*` 参数），但 REST API 可以在请求体中将新字段设为可选。
- hls.js 的 liveSyncPosition 在录制回放模式（`type: vod`）下仍然可用，这意味着同一套逻辑可以同时服务于直播和录播弹幕同步。

**不推荐的方案**：不要再引入一套独立的时间轴系统。利用现有的 RTP timestamp（90kHz 时钟）和 PCR（27MHz 时钟）之间的可换算关系（27MHz / 300 = 90kHz），统一到 90kHz tick 作为 canonical 媒体时间单位。

---

### 方向 E（优先级 P2）：Web 前端工程化起步

> 对应分析文档方向二。这不是架构核心问题，但产品化必须走的路。

**为什么需要**：当前前端是「调试客户端」级别（`web/index.html` 第 26 行自述），不能直接用于 alpha 发布。但后端已经 P0-P11 就位。前端生产化的缺失是当前产品化路径上最大的阻塞点。

**核心挑战**：重写的前端项目是独立工程，不是架构修改。需要考虑：

1. **增量替换 vs 大爆炸重写**：不建议一次性重写全部 ~5,900 行 JS。建议先做增量工程化：
   - 引入构建工具（vite 或 esbuild，保持零 TS 起步）
   - 将单文件 `app.js`（930 行）拆为模块
   - 引入最小状态管理（zustand 或类似，替代全局 mutable `state`）
   - 保留现有 ES module 结构，逐步迁移

2. **与后端的 API 兼容性**：当前 WS 帧 `ClientFrame`/`ServerFrame` 是强类型 JSON。前端重写时不应改协议，应该把 WS 帧处理抽象到一个独立的 `ws-client` crate（用 wasm-pack 编译为 wasm），让 Rust 后端和 wasm 前端共享帧定义。

3. **跨标签页状态同步**：**这是一个常被忽略但实现成本高的需求**。多标签页意味着：
   - 多条 WS 连接（服务端需要通过 `hub_id` 区分同一 participant 的多设备）
   - 本地状态同步（`BroadcastChannel` API 或 SharedWorker）
   - 推送通知去重（一个标签页处理了通知，其他标签页不应重复）

   不建议 P2 阶段实现完整的跨标签同步。P2 目标：单标签页可产品使用 + 离线消息 IndexedDB 缓存 + PWA 清单。

**预期架构变更**：

```
├── web/ → 重构为:
│   ├── package.json（vite + 最小依赖）
│   ├── src/
│   │   ├── main.js（入口，替换 app.js 的视图路由）
│   │   ├── ws/（ws.js 重构，IndexedDB 持久化 cursors）
│   │   ├── store/（zustand store，替代 context.js）
│   │   ├── views/（按页面拆分，替代 app.js + render.js）
│   │   ├── live/（live.js 重构）
│   │   ├── calls/（calls.js 重构）
│   │   └── sw.js（service worker）
│   ├── public/
│   │   └── manifest.json（PWA）
│   └── index.html（static shell，加载构建产物）
└── 可选的: crates/aero-ws-client（wasm-pack 共享 WS 帧类型）
```

**对现有系统影响**：最终 API 契约不变。REST + WS 帧结构作为设计合同，前端和后端各自演进。wasm-pack 的 `aero-ws-client` 是可选的，不是必选项。

---

## 3. 接口设计原则

### 3.1 关键模块接口原则

**（1）Hub 扇出接口：始终保持 `try_send` 语义**

`Hub::fan_out_raw` 当前是 bounded `mpsc` + `try_send`。这是正确的模式——扇出不阻塞 publisher 路径。方向 C 中引入 `FanOutStrategy` 时，**不要改变这个核心语义**：

```rust
// 当前（保持不变）
pub fn fan_out_raw(&self, room_id: RoomId, event: &ServerFrame) -> FanOutResult;

// 扩展后（新增重载，不是修改签名）
pub fn fan_out_strategic(
    &self,
    room_id: RoomId,
    event: &ServerFrame,
    strategy: FanOutStrategy,    // 新枚举
) -> FanOutResult;
```

`FanOutStrategy` 应该是 `Hub` 的纯数据输入，不包含逻辑。策略的判断应该在调用方（`handle_room_event_sub`）完成，`Hub` 只负责扇出。

**（2）NATS 事件格式：扩展勿破坏 serde 向后兼容**

当前 `RoomEvent` 用 `#[serde(tag="kind")]`。新增字段必须为 `Option` 或 `#[serde(default)]`。方向 D 的 `StreamChatLine` 扩展：

```rust
// 当前
pub struct StreamChatLine {
    pub id: MessageId,
    pub server_timestamp: DateTime<Utc>,
    // ...
}

// 扩展后
pub struct StreamChatLine {
    pub id: MessageId,
    pub server_timestamp: DateTime<Utc>,
    #[serde(default)]
    pub media_tick: Option<i64>,     // 新，旧消息反序列化为 None
    #[serde(default)]
    pub media_epoch: Option<i32>,    // 新
    // ...
}
```

**（3）仓储层：保持 `repo-per-module` 模式**

当前仓储层每个功能模块一个 `XRepo`（`DeliveryCursorRepo`、`StreamChatSettingsRepo` 等），都在 `aero-storage` 内。这比 single-repo-with-all-methods 的模式好，但注意各 `XRepo` 之间不要循环引用。如果需要跨仓储事务（如 `dispatch_notifications` 需要 `NotificationRepo` + `MuteRepo` + `PresenceRepo`），应该在上层（`ImService` 或 `LiveService`）组合，仓储层只提供 `&mut Transaction` 的方法。

### 3.2 是否需要新抽象层

| 领域 | 是否需要新抽象 | 理由 |
|---|---|---|
| 消息扇出策略 | 是——`FanOutStrategy` trait | 避免在 `handle_room_event_sub` 中堆叠 if-else |
| 媒体时间戳提供 | 是——`MediaTimestampProvider` trait | 屏蔽 WHIP/RTMP/SRT 的时间戳差异 |
| 广播通知治理 | 否 | 复用已有 `rate_limiter` 设施即可 |
| WebSocket 协议层 | 否 | 只在当前 WS 帧定义内扩展，不加新协议层 |
| 推送门控 | 否 | 当前 `push_bot` 的 `Rejected→unregister` 模式已够用 |

**`FanOutStrategy` 的备选设计考虑**：

```rust
#[non_exhaustive]
pub enum FanOutStrategy {
    /// 只扇出到在线成员（消息不 @everyone）
    OnlineOnly,
    /// 扇出到所有成员（消息 @everyone 或命令型）
    AllMembers {
        /// 如果 true，即使 fans-out 给在线成员，也发送 NotifyBatch 给离线
        notify_offline: bool,
    },
    /// 只扇出到显式收件人列表（消息 @user 或 reply）
    Explicit(Vec<ParticipantId>),
    /// 扇出到线程订阅者（消息在线程内）
    ThreadSubscribers,
}
```

### 3.3 向后兼容性策略

1. **REST API**：新请求体字段为 `Option`，默认值为 `None`（退化为当前行为）。从不移除旧字段。
2. **WS 帧**：`ServerFrame` 新变体通过 `#[serde(deny_unknown_fields)]` 保护——不对，当前是 `#[serde(tag="kind")]`，新变体在客户端旧代码中会被静默丢弃（因为接收方 match 没有该 arm）。这是设计意图吗？需要确认客户端是否有兜底 arm（`_ => {}`）。查看 `web/ws.js`：
   ```js
   ws.on('msg:stream_event', data => ...);
   ```
   如果新事件类型没有注册 handler，就是静默丢弃。**建议为所有 WS 事件注册一个兜底日志**，方便调试期识别客户端版本落后于服务端。
3. **迁移**：`ADD COLUMN ... DEFAULT NULL` 永远向后兼容。不要用 `NOT NULL` 加新列。
4. **NATS subject**：不改变 subject 命名规则。新 subject 分到新的 consumer group。

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

| 方向 | 新引入 | 已有替代 | 建议 |
|---|---|---|---|
| 方向 A: 重连 TOCTOU | 无 | `delivery_cursor` 迁移已建 | 不引入 |
| 方向 B: 通知风暴 | Redis sorted-set 做冷却计数器 | 已有 `rate_limiter`（`AERO_RATE_LIMIT_PER_SEC`） | 复用 |
| 方向 C: 写放大控制 | 可能需要 per-room 在线 Redis set | 已有 `presence`（`LivePresenceStore`） | 复用 + 扩展 |
| 方向 D: 弹幕时序 | 无 | 扩展 `StreamChatLine` 字段 | 不引入 |
| 方向 E: Web 前端 | Vite / esbuild（构建）+ zustand（状态） | 无 | 引入（最小） |

**结论**：5 个方向中，只有方向 E 需要引入新的前端构建工具链。其余四个方向全部复用以有基础设施。

### 4.2 第三方依赖评估标准

Rust 生态系统评估准则：

| 准则 | 问法 | 解释 |
|---|---|---|
| 版本锁定 | 是否已在 `Cargo.lock` 中？ | 新依赖必须至少有一个 crate 已在 workspace 中引用其传递依赖，否则需要全量 audit |
| unsafe 使用 | `cargo geiger` 检查 | 结合 workspace lints（`unsafe_code = "forbid"`），仅允许 `unsafe` 排除后的 crate |
| str0m 限制 | 仅限 `aero-live-whip`/`aero-live-webrtc` Cargo.toml | root `Cargo.toml` 不加 str0m 依赖（AGENTS.md §4.5） |
| 编译时间 | 新 crate 的 `syn`/`quote`/`proc-macro2` 使用 | 编译时间敏感度：每次 `cargo check --workspace` 应 ≤30s |

### 4.3 自建 vs 采购

当前系统设计理念清晰——**所有核心能力自建，辅助能力通过库集成**：

- NATS JetStream（自运维，非 SaaS）— 正确。IM 实时是核心竞争力的保底基础设施，不应依赖第三方流转
- AI Gateway（自建 Anthropic / Voyage 包装）— 正确。AI 能力是差异化卖点，控制 prompt 模板和 budget
- FCM/APNs 推送（自建 gateway + `FakeGateway`）— 正确。移动推送需要私密的无 token 泄露路径
- S3/MinIO 附件（支持策略选择）— 正确。客户可能要求 S3-compatible 自托管

**不需要变更此策略**。即使方向 C 需要写放大控制，也完全是代码架构层面的优化，不需要外部 SaaS。

---

## 5. 实施路线图

### 5.1 优先级排序

| 优先级 | 方向 | 估计工作量 | 风险 | 依赖 |
|---|---|---|---|---|
| **P0** | B. @everyone 通知风暴防护 | 1-2 天 | 低 | 无 |
| **P0** | A. 重连 TOCTOU + delivery cursor 全线接入 | 3-5 天 | 中 | 需要前后端同步修改 |
| **P1** | D. 弹幕媒体时间戳 | 5-7 天 | 中 | WHIP/RTMP/SRT 三种 adapter |
| **P1** | C. 写放大控制 Phase 1（在线优先 + buddy cache） | 5-10 天 | 中 | P0 完成后确保基础稳定 |
| **P2** | C. 写放大控制 Phase 2（FanOutStrategy + 离线回填队列） | 10-15 天 | 高 | Phase 1 验证后 |
| **P2** | E. Web 前端工程化起步 | 15-30 天 | 高 | 独立项目，与后端解耦 |

### 5.2 阶段划分

**Phase 0（1 周）—— 运营安全加固**

```
Week 1:
  ├── @everyone 冷却（P0）
  │   ├── per-room sorted-set 计数器（Redis，复用 presence key 空间）
  │   ├── @here 降级告警 metrics
  │   └── broadcast API 的冷却复用（非 @everyone 但也需防护）
  ├── delivery cursor 接入（P0）
  │   ├── ws.js: cursors 参数序列化（per-room map → JSON → base64）
  │   ├── mod.rs: parse_resume_cursor → parse_resume_cursors
  │   ├── DeliveryCursorRepo: batch_fetch + batch_upsert
  │   └── SeqGate: 确认 backfill 路径 seq 命名空间统一
  └── 发布 v0.1.0-alpha（可 internal alpha 发布）
```

**Phase 1（2 周）—— 直播体验 + 扩展性 Phase 1**

```
Week 2-3:
  ├── 弹幕媒体时间戳（P1）
  │   ├── 迁移: 新增 stream_chat 媒体时间字段
  │   ├── MediaTimestampProvider trait + WHIP adapter
  │   ├── RtmpSession adapter（FLV timestamp→90kHz tick）
  │   ├── SrtSession adapter（保留 TS PCR）
  │   ├── LiveService::post_chat/post_gift 扩展
  │   └── web/live.js: MediaTimelineSync
  ├── 写放大控制 Phase 1（P1）
  │   ├── RoomMemberCache: 在线成员过滤缓存
  │   ├── handle_room_event_sub: 优先在线列表
  │   ├── NotifyBatch: 分段发送（>1000 收件人）
  │   └── dispatch_notifications: 收件人子集优先
  └── 发布 v0.2.0-beta（可 beta 发布）
```

**Phase 2（3-6 周）—— 深度优化 + 前端工程化**

```
Week 4-9:
  ├── 写放大控制 Phase 2（P2）
  │   ├── FanOutStrategy trait + 默认实现
  │   ├── OfflineBackfillQueue（懒加载离线消息）
  │   └── Hub::fan_out_strategic（新接口）
  ├── Web 前端工程化（P2）
  │   ├── vite 初始化 + ESM 模块拆分
  │   ├── zustand 状态管理（替换 context.js）
  │   ├── IndexedDB 持久化 cursors + 消息缓存
  │   ├── PWA manifest + service worker（离线缓存 shell）
  │   └── 与后端 API 兼容性验证
  └── 发布 v0.3.0（public alpha）
```

### 5.3 风险点与缓解

| 风险 | 影响 | 概率 | 缓解 |
|---|---|---|---|
| delivery cursor 合并语义缺陷 | 重连丢失消息 | 中 | 在 Phase 0 中增加 backfill 路径的 SeqGate 双重验证 + 测试 |
| per-room sorted-set 增长失控 | DoS 放大器 | 低 | 复用 `RateLimitSweeper` 空闲驱逐；`COUNT(*)=0` 时 `UNLINK` |
| 媒体时间戳 PCR 回绕未处理 | 大于 26.5h 直播弹幕定位错位 | 中 | `media_epoch` 计数 + 设计评审时强制 review |
| RTMP 无 RTP 时间戳 | RTMP 推流无法媒体同步 | 高（RTMP 协议限制） | FLV timestamp 推算 + 降级 wall-clock 同步 |
| 前端工程化范围蔓延 | 工期超出估计 | 高 | 严格限定 P2 范围：不重构 UI，只做工程化 + 状态管理 + 离线缓存 |
| 写放大 Phase 2 的 FanOutStrategy 影响 Hub 扇出路径 | 消息丢失 | 高 | Phase 2 之前需要 Phase 1 的稳定性验证 + 灰度部署 |

---

## 6. 补充建议（prompt 中未覆盖但值得提及的）

### 6.1 Redis 键空间治理

当前 `presence`、`StreamViewerStore`、`CallRosterStore`、未来的 `@everyone` 冷却计数器都在 Redis sorted-set 中。键的命名约定需要统一：

```
presence:workspace:{wid}         # 已有
live:viewers:stream:{sid}        # 已有
call:roster:call:{cid}           # 已有
rate:everyone:room:{rid}         # 新增
rate:everyone:sender:{uid}:room:{rid}  # 新增（可选）
```

新增的两个键应该明确过期时间（TTL = cooldown_window + 1 tick），避免 stale key 堆积。

### 6.2 `hub_id` 的标准化

当前 Hub 是 per-process 实例，但 WS 连接上没有显式的 `hub_id` 标签。如果 Phase 2 的 `NotifyBatch` 分段需要按 `hub_id` 路由，需要在 WS 握手时分配一个 UUID 给 Hub，并让 NATS consumer 可以按 `hub_id` 过滤。

这不需要新基础设施——WS upgrade 时在服务端通过 Axum `Extension<HubId>` 注入即可。`NotifyBatch` 的可选路由 hint 是：

```rust
pub struct NotifyBatch {
    pub recipients: Vec<ParticipantId>,
    pub routing_hint: Option<HubId>,    // None = 所有 Hub 处理
}
```

### 6.3 测试策略升级

当前 841 个 hermetic 测试主要是仓储层 + service 层单测。方向 A 和 D 涉及**端到端协议交互**（WS upgrade → backfill → live 扇出 → SeqGate 去重），当前单测覆盖不到。建议为这两个方向增加的测试类型：

```
方向 A: 
  ├── 集成测试: WS reconnect → backfill → live 并行投递 → 去重验证
  └── 属性测试: 随机关停/重连下的消息收敛验证（模拟 mpsc 丢消息）

方向 D:
  ├── 集成测试: WHIP 推流 → RTP ts 捕获 → StreamChatLine.media_tick 写入
  └── 集成测试: HLS 播放 → PCR 映射 → 弹幕显示时间计算
```

### 6.4 prompt 中的 SeqGate 双路径去重验证

prompt 提到：「如果游标回填和 live 扇出都走通，双路径可能重复投递，需要 SeqGate 在客户端做最终去重。」

我的补充建议：不仅客户端需要 SeqGate，服务端的 backfill 路径也应该做防重。具体来说，backfill 查询时**排除** participant 已通过游标确认到达的消息：

```sql
-- 当前: 查 > cursor_seq 的所有消息
SELECT * FROM messages
WHERE room_id = $1 AND seq > $2
ORDER BY seq ASC
LIMIT 200;

-- 潜在问题: backfill 的 seq 和 live 扇出的 seq 同 namespace 吗？
```

如果 per-subject seq 是单调递增且全局唯一的（在 NATS subject 级别），backfill 和 live 两条路径的 seq 空间应该重合，SeqGate 在客户端就能去重。但如果 backfill 路径使用了不同的 seq 来源（比如消息的 `created_at` 或 DB 自增 id），SeqGate 就失效了。**需要确认 `backfill_since` 的查询是按 `seq` 还是 `created_at`**。

根据代码 `backfill_since` 查询的是 `messages.created_at > M0`——这意味着 backfill 路径没有用 `seq`。引入 `delivery_cursor` 后，应该改为按 `seq` 回填：

```sql
SELECT * FROM messages
WHERE room_id = $1 AND seq > $2    -- 改用 seq
ORDER BY seq ASC
LIMIT 200;
```

这样回溯和 live 两个路径的 seq 在客户端 SeqGate 中就能正确合并。

---

## 总结

| 维度 | 结论 |
|---|---|
| 当前架构强度 | 事件驱动骨架正确，crate 层级干净。写路径 O(N) 在超大房间面临瓶颈 |
| 最优先行动 | @everyone 冷却（1 天）+ delivery cursor 接入（3-5 天） |
| 技术风险最高 | 弹幕媒体时间戳的 RTMP/SRT adapter（协议限制） |
| 产品化阻塞 | Web 前端生产化（方向 E），但这是独立工程，与架构改造可并行 |
| 不应做的事 | 不引入新基础设施依赖（所有方向复用现有 PG/Redis/NATS）、不做 E2E 加密、不做联邦 |
| 架构统一主题 | 当前有很多「已建但未接」的 seam（delivery cursor、MediaTimestampProvider），最优先的事是把这些 seam 接上，而不是开新方向 |
