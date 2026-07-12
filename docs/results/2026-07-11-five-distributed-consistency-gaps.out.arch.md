# 架构分析：Aero IM 分布式一致性裂缝

> 基于 `2026-07-11-five-distributed-consistency-gaps.md` 及其代码级验证结论 `.out.md`，从架构师视角进行系统性评估。
>
> **分析视角**：不聚焦单个 bug 修复，而是识别模式、根因、以及系统级的演进方向。

---

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 的架构在几个关键维度上做出了**正确的顶层决策**：

| 决策 | 优点 | 代价（与当前裂缝的关系） |
|------|------|-------------------------|
| **NATS JetStream 作为事件总线** | 跨实例扇出天然解耦；持久化消费者提供 at-least-once 保证；独立游标使慢消费者不阻塞快消费者 | 独立消费者游标 = 跨消费者无顺序保证 → **方向一** |
| **PG 作为单一事实源** | 事务性写入保证消息/房间/成员状态一致；MVCC 支持并发读取 | 搜索索引异步更新导致最终一致性窗口 → **方向二** |
| **Redis 作为集群级状态** | 低延迟跨节点路由/presence；sorted-set 天然支持过期驱逐 | 与进程内缓存的 TTL 不一致 → **方向四** |
| **进程内 HUB + DashMap** | WebSocket 扇出零网络往返；O(1) 连接注册/注销 | 跨节点状态不可见；缓存 TTL 窗口是安全边界 → **方向四** |
| **SFU 进程内内存状态** | 媒体转发零延迟决策 | 与 Redis 路由的双写不一致 → **方向三** |

**核心权衡**：系统选择了**高吞吐、低延迟、水平扩展**的路径，为此承受了**最终一致性窗口**。这套权衡在大多数场景是合理的，但在五个边界上超出了可接受阈值。

### 1.2 关键设计决策评估

#### 决策一：独立 durable consumer 模式（方向一、方向五）

当前架构中，WS 扇出、Webhook、审核、Bot、推送各持一个独立 durable consumer。这个模式本身没有错——它实现了优雅的关注点分离。但**缺少跨消费者的因果协调层**导致了方向一的 P0 问题。

```
                   im.room.{id}
                        │
       ┌────────────────┼────────────────┐
       ▼                ▼                ▼
  aero-server     aero-moderation    aero-webhooks
  (cursor: 42)    (cursor: 38)       (cursor: 45)
       │                │                │
       ▼                ▼                ▼
    WS fan-out       AI 审核         HTTP POST
    (最新)           (滞后)           (已超前)
```

**问题**：三个消费者各自独立推进，审核滞后但 Webhook 已超前。审核命中 → 发布 `Deleted` → Webhook 已经发出了未审核的原始消息。

**评估**：这个设计对于"无所谓谁先谁后"的事件是对的。但对于**存在因果依赖的事件链**（消息 → 审核 → 删除/放行），缺少屏障机制是架构级疏漏。**需要提升为 P0 修复**（验证结论已确认）。

#### 决策二：TTL 缓存的"下游 auth gate"假设（方向四）

`room_member_cache.rs` 注释明确写道："the member / auth gate downstream filters anyway"——假设下游有权限校验。但验证发现 `hub.fan_out_raw` 路径**没有**下游 auth gate。

这是**架构契约断裂**：缓存层依赖于一个不存在的守卫。缓存 TTL 一致性窗口本是一个性能优化，但在缺少下游验证的情况下变成了安全漏洞。

**评估**：这是注释与实际实现的偏差。修复可以走两个方向：
1. **增加下游 auth gate**（在 `fan_out_raw` 前校验每个 recipient 是否仍为成员）——但会引入 O(N) DB 查询
2. **降低 TTL + Redis Pub/Sub 失效广播**——保持性能的同时缩小窗口

#### 决策三：异步 embedding 更新路径（方向二）

AI-Native IM 的核心卖点是 RAG 搜索。但 `edit_message` 后的 embedding 更新走异步 `ai_jobs` 队列，延迟不确定（最多可达 60s 预算等待）。

**评估**：对于企业合规场景（用户编辑了一条违规消息，期望搜索立即不再返回），60s 窗口太长。可以考虑**同步路径 + 异步路径并存**的策略——小消息同步更新 embedding（预算允许），大消息或高并发时回退异步。

#### 决策四：通话状态跨四存储（方向三）

通话生命周期横跨 PG、Redis、SFU 内存、call_bridge 内存四个状态存储，无分布式事务或 Saga。这是最复杂的多写模式。

**评估**：通话场景即使用户体验要求高（不能接受"加入了但听不到"），但考虑到通话在 Aero IM 的使用频率可能低于消息，且"部分失败"在跨节点场景是常态，**SaGa 模式 + 定期对账**比引入分布式事务更务实。

### 1.3 架构债务确认

| 债务项 | 严重度 | 说明 |
|--------|--------|------|
| `fan_out_raw` 无下游权限校验 | **P1（安全）** | 被踢成员 60s 超扇出窗口，验证结论建议从 P2 升至 P1 |
| `handleEdited` append 幽灵消息 | **P0（数据面）** | 实际行为比原文档描述的更严重——不是静默跳过，而是 append DOM |
| 消费者隔离无因果屏障 | **P0（架构）** | 审核/Webhook/Bot 与 WS 扇出的时序无法保证 |
| 无 Saga 补偿的通话多写 | **P1** | 部分加入/离开导致僵尸路由或静默失败 |
| 搜索索引的最终一致性无用户提示 | **P2** | 用户看到搜索结果与消息内容不匹配 |
| 推送与 WS 无协调 | **P2** | 桌面活跃时手机仍收到推送 |

---

## 2. 扩展方向

### 方向一（推荐 · 优先）：事件因果屏障层

> **方向一原文**为 "事件交付顺序"，但这里的扩展方向是架构层的解决方案，非修补。

#### 为什么需要

方向一 P0 风险的根本原因是：独立消费者之间的因果关系未显式表达。当前的"先到先得"模型在审核、Webhook、Bot 三个场景都会产生用户可见的时序错乱。这是**数据面完整性**问题，不是性能优化。

#### 核心挑战

1. **Barrier 事件的定义**：需要在 ROOM subject 上插入一个屏障标记，要求所有消费者在继续推进前等待屏障被"解除"（类似于 MPI_Barrier 或分布式事务的 Prepare 阶段）
2. **性能影响**：如果每个消息都带屏障，吞吐会大幅下降。需要只对特定事件类型（审核决策）加屏障
3. **与现有 at-least-once 模型的兼容**：NATS 不原生支持跨消费者屏障，需要应用层实现

#### 建议方案

有三种选择，各有权衡：

**选项 A：审核同步化（推荐）**

将 `moderation_bot` 移入 WS fan-out 的同一进程上下文，使审核判决发生在事件扇出之前：

```
Message → 审核判决（同步）
  ├─ 通过 → fan_out_raw（WS 扇出）
  └─ 命中 → soft_delete → 不扇出（或扇出 Deleted）
```

- ✅ 审核时序问题彻底解决
- ✅ 不需要屏障协议
- ❌ `moderation_bot` 失去了独立扩展性（不能单独扩容审核消费者）
- ❌ 审核延迟会阻塞消息扇出（需配置超时 Fail-open）

**选项 B：事件 seq 屏障（通用）**

在 `im.room.{id}` 上引入 `Barrier` 事件类型。模块 `aero-server` 在发布 `Barrier` 后，所有消费者必须先处理完 barrier 之前的事件，才能继续处理 barrier 之后的事件。客户端利用已有 `SeqGate` 排序。

```
Event: Message(seq=5)
Event: Message(seq=6)
Event: Barrier(seq=7, wait_for=[moderation_bot, webhook])
Event: Message(seq=8)
```

- ✅ 通用方案，所有消费者受益
- ✅ 客户端 SeqGate 已就绪（ws.js:18-39）
- ❌ 实现复杂，需要每个消费者在 Barrier 处同步
- ❌ 增加延迟（快消费者等待慢消费者）
- ❌ NATS 不原生支持，需应用层轮询状态

**选项 C：在扇出前做轻量权限校验（补丁而非架构方案）**

在 `hub.fan_out_raw` 入口处增加 `room_member_cache` 校验——在扇出前查询 recipient 是否仍为房间成员。结合 Redis Pub/Sub 失效广播，将 TTL 窗口降至 ~1s。

- ✅ 方向四 P1 安全漏洞同步修复
- ✅ 实现成本低
- ❌ 不解决方向一的审核→交付时序问题
- ❌ 不解决 Webhook 顺序问题

**我的建议**：**先做 C（低风险稳安全）+ 开始 A 的评估（高影响核心修复）**。B 方案虽优雅但实施成本高、收益回报期长，适合全系统第二阶段演进。

### 方向二：搜索索引可见性层

#### 为什么需要

RAG 搜索是 Aero IM 的 AI-Native 核心差异化能力。当前方向二的问题（编辑后搜索返回旧内容、删除后搜索仍返回）直接损害用户信任，在企业合规场景下有法律风险。

#### 核心挑战

1. **同步 vs 异步的决策**：小消息数千条/秒，全部同步更新 embedding 会压垮 AiWorker
2. **embedding 版本一致性**：搜索时需要知道 embedding 是否与 `searchable_text` 对齐

#### 建议方案

**分层索引策略**：

```
消息编辑
  │
  ├─ 同步路径（小消息，预算允许）:
  │   更新 searchable_text → 同步更新 embedding（ai_jobs 队列优先通道）
  │   
  ├─ 异步路径（大消息/高并发/预算耗尽）:
  │   更新 searchable_text → enqueue Embed → 设置 embedding_stale=true
  │
  └─ 搜索结果过滤:
       在查询时检查 embedding_stale 标志
       └─ stale=true → 降级到 FTS-only（跳过向量检索）
                         同时在结果中标记 "搜索索引同步中"
```

关键组件：
- `messages.embedding_stale` 布尔列（NOT NULL DEFAULT false）
- 搜索查询时：`WHERE NOT embedding_stale AND deleted_at IS NULL`
- 搜索结果中：对 `embedding_stale=true` 的消息标记 "内容已更新，索引同步中"
- 删除路径同步清除：`SET embedding = NULL, embedding_stale = true`

### 方向三：通话生命的 Saga + 对账

#### 为什么需要

通话状态跨 4 个存储，部分失败会导致用户（加入通话但听不到声音）和资源（僵尸 SFU peer）的问题。跨节点部署时，网络抖动会使部分失败从异常变为常态。

#### 核心挑战

1. **Saga 补偿的幂等性**：每个操作（add_peer、register_route）都需幂等 ID，补偿操作需能安全重试
2. **对账循环的性能**：全量对比 Redis 路由和 SFU peer 在大规模部署下可能很慢
3. **发现不一致后的修复策略**：以哪个存储为准（建议 Redis 为准）

#### 建议方案

**Saga 模式修正 `join_group_call`**：

```text
join_group_call():
  ① add_peer(SFU)          ── 失败 → 回滚（cleanup local state）
  ② register_route(Redis)  ── 失败 → compensate: remove_peer(SFU)
  ③ decide_topology        ── 失败 → compensate: unregister_route(Redis) + remove_peer(SFU)
```

**定期对账（call_bridge_supervisor 的思想可复用）**：

```
每 30s（与 heartbeat 同周期）:
  for each active call:
    redis_peers = CallRouteStore::participants(call)
    sfu_peers = SfuRouter::peers(call)
    
    // Redis 有但 SFU 无 → 僵尸路由，清理 Redis
    for p in redis_peers - sfu_peers:
      CallRouteStore::unregister(call, p)
    
    // SFU 有但 Redis 无 → 孤儿 peer，清理 SFU（该节点可能即将关闭）
    for p in sfu_peers - redis_peers:
      SfuRouter::remove_peer(call, p)
```

**验证结论中的修正**（leave 顺序是 SFU→Redis，不是 Redis→SFU）：

当前顺序是 `SFU.remove_peer` → `Redis.unregister`。如果第一步成功但第二步失败，Redis 有僵尸路由。这与原文档描述相反。**建议保持当前顺序**（先清理 SFU 再清理 Redis），因为：
- SFU 是本地进程内状态，失败概率低
- Redis 可被对账循环清理
- 僵尸路由（Redis 有、SFU 无）比对等 peer（SFU 有、Redis 无）更安全——路由指向一个不存在的人，对方不会收到媒体

### 方向四：缓存一致性升级

#### 为什么需要

验证结论已确认：`room_member_cache` 注释声称的下游 auth gate 实际不存在。被踢成员继续收到消息是真实安全漏洞，建议从 P2 升级到 P1。

#### 核心挑战

1. **一致性窗口的根源**：60s TTL 是性能与一致性的折中，但在安全场景不可接受
2. **跨节点缓存失效**：当前 `invalidate` 只影响本节点，其他节点需等待 TTL 过期

#### 建议方案

**Redis Pub/Sub 失效广播**（轻量级实现）：

```
成员变更（add_member / remove_member）:
  ① PG 事务写入 room_members
  ② room_member_cache.invalidate（本节点）
  ③ Redis PUBLISH room:invalidate:{room_id} timestamp

所有节点监听 room:invalidate:*:
  收到消息 → 本节点 room_member_cache.invalidate(room_id)
```

这个方案：
- ✅ 已有 Redis 连接（复杂度低）
- ✅ 不改变 TTL 逻辑（即使 Pub/Sub 丢消息，TTL 是保底）
- ✅ 变更窗口从 60s 降至 ~网络延迟（<100ms）
- ✅ 不增加消息扇出路径的延迟

**结合扇出前校验**（`fan_out_raw` 入口处校验 `room_member_cache`）：

```rust
pub fn fan_out_raw(&self, recipients: &[ParticipantId], text: &str, room_id: RoomId) {
    let still_members = self.room_member_cache
        .get_or_fetch(room_id)
        .await
        .unwrap_or_default();
    
    let valid_recipients: Vec<_> = recipients
        .iter()
        .filter(|p| still_members.contains(p))
        .collect();
    
    // 只扇出给仍为成员的 participants
    Self::fan_out_arc_inner(self, &valid_recipients, ...);
}
```

⚠️ 注意：这需要在 `fan_out_raw` 签名中传递 `room_id`，或者将 room_id 编码在事件中。当前 `bus.rs` 的 `handle_room_event_sub` 已经能拿到 `room`，可以在那里过滤。

### 方向五：推送-已读-Presence 协调层

#### 为什么需要

推送通知的用户体验问题直接影响留存率和产品口碑。"桌面已读但手机仍收到推送"是用户最常投诉的推送痛点之一。这条方向虽无安全风险，但有**商业价值**。

#### 核心挑战

1. **推送的最终性**：FCM/APNs 一旦发出无法撤销
2. **跨设备已读状态的延迟**：`mark_read` 在 PG 和 Redis 之间可能不同步
3. **聚合逻辑的复杂性**：将多条通知合并为一条需要缓存等待窗口

#### 建议方案

**Presence 感知推送**（最低成本、最高收益）：

```
push_bot 发送前:
  ① 查询 Redis presence（3s 内最后活跃时间）
  ② 查询 participant_cache（该用户是否有活跃 WS 连接）
  ③ 如果 desktop_active → 跳过推送（静默推送更新角标）
  ④ 如果 mobile_only → 正常推送
```

**DND/snooze 门控前移**：

当前 `push_bot` 不检查 `notif_prefs`。这是明显的 bug。可以在 `push_bot::handle_notify` 入口处增加：

```
① 查询 notif_prefs（DND 时段/静音房间列表）
② 查询 snooze 状态
③ 如果 DND/静音/snooze → 跳过推送
```

---

## 3. 接口设计建议

### 3.1 关键模块接口原则

#### RoomEvent 总线契约（方向一、四相关）

当前 `RoomEvent` 通过 `serde(tag="kind")` 序列化，每个 variant 包含完整的消息体。建议新增以下字段（向后兼容）：

```rust
#[serde(tag = "kind")]
enum RoomEvent {
    Message {
        // 现有字段...
        
        // 新增（可选，向后兼容）
        #[serde(default)]
        causal_barrier: Option<CausalBarrier>,  // 因果屏障标记
        
        #[serde(default)]
        room_id: RoomId,  // 当前附在 message.room_id，顶层显式化
    },
    // ...
}
```

**设计原则**：所有可选字段使用 `#[serde(default)]` 确保老版本服务器/客户端不崩溃。

#### CausalBarrier 接口（方向一解决方案）

```rust
/// 因果屏障：消费者在继续处理后续事件前，必须确认屏障前的所有事件已处理完毕。
struct CausalBarrier {
    /// 屏障类型
    kind: BarrierKind,
    /// 屏障前的最后事件 seq
    seq: u64,
    /// 需要等待的消费者列表
    wait_for: Vec<String>,  // 消费者名称，如 ["aero-moderation", "aero-webhooks"]
}

enum BarrierKind {
    /// 审核决策（命中/放过）
    ModerationDecision,
    /// Webhook 触发
    WebhookTrigger,
}
```

### 3.2 是否需要新抽象层

**是的，需要两个新抽象层**：

#### 层一：事件因果协调层（Event Causality Coordinator）

```
当前： RoomEvent → NATS → 各消费者独立处理
建议： RoomEvent → ECC → NATS + 屏障 → 各消费者
```

职能：
- 识别需要跨消费者保证顺序的事件链
- 插入 Barrier 事件
- 轮询各消费者的确认状态
- 超时 Fail-open（不因一个消费者阻塞全系统）

#### 层二：通话状态 Saga 管理器

```
当前： join_group_call → 4 个独立写操作
建议： CallSagaManager → 协调 4 步 Saga + 补偿
```

### 3.3 向后兼容策略

所有接口变更必须向后兼容，因为：

1. **NATS 消息序列化格式**：老消费者不能因未知字段崩溃
   - 使用 `#[serde(deny_unknown_fields)]` 在顶级校验但 variant 内允许未知字段
   - 或使用 `#[serde(default)]` 为新字段提供默认值

2. **WebSocket 帧格式**：老 Web SPA 不能因新字段崩溃
   - 当前做法正确：`extra data is ignored`
   - 新事件类型使用独立 subject/type

3. **REST API**：使用 JSON Merge Patch 语义（新字段无值时不影响老客户端）

---

## 4. 技术选型

### 4.1 是否需要新技术栈？

**不建议引入新的基础设施组件**。当前的技术栈（NATS、Redis、PG、SFU）可以覆盖所有五个方向的修复：

| 方向 | 所需技术 | 是否已就绪 |
|------|---------|-----------|
| 方向一 | 应用层屏障协议 | NATS JetStream 已就绪，需在应用层实现 |
| 方向二 | embedding 版本控制 | PG + AiWorker 已就绪，新增列 |
| 方向三 | Saga + 对账 | 当前 call_bridge_supervisor 模式可复用 |
| 方向四 | Redis Pub/Sub | Redis 已就绪，仅需新增 PUB/SUB channel |
| 方向五 | Presence + DND 查询 | Redis presence + notif_prefs 已就绪 |

**唯一的候选引入**：如果选择方向一的"审核同步化"选项——需要将 `moderation_bot` 从独立 task 移入 WS fan-out 进程，**不引入新技术，只是架构重组**。

### 4.2 第三方依赖评估标准

当前依赖清单中的关键依赖评估：

| 依赖 | 风险评估 | 建议 |
|------|---------|------|
| **async-nats 0.36** | JetStream API 不稳定（0.x），升级需测试 | 锁版本，升级依赖于 NATS Server 版本匹配 |
| **str0m 0.19** | 纯 Rust DTLS/SRTP/RTP 栈，主线活跃但 API 不稳定 | 锁版本，非 root 依赖（仅 `aero-live-webrtc`、`aero-live-whip`） |
| **fred 9 (Redis)** | 成熟，API 稳定 | 无担忧 |
| **sqlx 0.8** | 成熟，编译时 SQL 检查是重大价值 | 保持 |
| **dashmap** | 轻量，成熟 | 适合进程内 Hub，但跨节点场景不要依赖它 |

### 4.3 自建 vs 采购

- **审核管道**：自建（当前基于 Anthropic API 的 AI 审核），不采购第三方审核服务
- **推送网关**：自建（`aero-push` 的 FCM/APNs 网关已就绪），不采购推送聚合服务（如 OneSignal）
- **向量搜索**：自建（PG + pgvector），不引入专门的向量数据库（如 Pinecone、Weaviate）。原因：
  - pgvector 嵌入 PG，零额外运维成本
  - 1024 维 embedding + GIN 索引的组合路径已在产品中验证
  - 团队已掌握 pgvector 的运维经验

---

## 5. 实施路线图

### 5.1 优先级排序（修正后）

基于验证结论的修正建议：

| 优先级 | 方向 | 修正理由 |
|--------|------|---------|
| **P0** | 方向一（事件交付顺序） | 审核→删除→交付时序错乱是数据面安全漏洞 |
| **P0** | 方向四中 `fan_out_raw` 无 auth gate | 验证结论建议 P2→**P1**，但我认为安全越权应**升为 P0** |
| **P1** | 方向三（通话跨存储一致） | 用户体验 + 僵尸资源累积 |
| **P1** | 方向二（搜索索引一致） | AI-Native IM 核心差异化功能 |
| **P2** | 方向五（推送协调） | 非安全，非功能完整性，但影响用户留存 |
| **P2** | 方向四中跨节点缓存一致 | Redis Pub/Sub 失效广播是纯优化 |

### 5.2 阶段划分

#### 阶段一（2-4 周）：止血——修复验证中发现的明确 bug

| 项目 | 涉及模块 | 风险 |
|------|---------|------|
| `handleEdited` 追加 DOM 修复（方向一） | `web/app.js:800-804` | 低——加 `idx >= 0` 守卫 |
| `fan_out_raw` 入口成员校验（方向四） | `hub.rs` + `bus.rs` | 中——需 room_id 传递到扇出路径 |
| `push_bot` 增加 DND/snooze 检查（方向五） | `push_bot.rs` | 低——只加查询逻辑，不改推送流程 |
| 删除路径同步清理 embedding（方向二） | `messages.rs:delete_message` | 低——单行 SET |

#### 阶段二（3-6 周）：架构修复——方向一 + 方向三

| 项目 | 方案选项 | 风险 |
|------|---------|------|
| 事件因果屏障（方向一） | 选项 C（扇出前校验）先行，选项 A（审核同步化）评估 | 高——屏障机制的实现和性能影响需要 prototyping |
| 通话 Saga 补偿（方向三） | 在 `join/leave/end` 方法中嵌入补偿逻辑 | 中——需要定义幂等 ID 生成策略 |
| 定期对账循环方向三） | 复用 call_bridge_supervisor 的 heartbeat 框架 | 低——已有可参考的模式 |

#### 阶段三（2-4 周）：搜索索引 + 推送协调 + 缓存优化

| 项目 | 涉及模块 | 风险 |
|------|---------|------|
| embedding 版本号 + 搜索降级（方向二） | `messages` 表 + `search.rs` | 低——新增列 + 查询条件 |
| Redis Pub/Sub 失效广播（方向四） | `room_member_cache.rs` + Redis | 低——复用现有 Redis 连接 |
| Presence 感知推送（方向五） | `push_bot.rs` | 低——查询 Redis presence 后再推送 |

### 5.3 风险矩阵

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| 屏障机制引入性能瓶颈 | 中 | 高 | 选项 C 先上；屏障只对审核事件生效，不做全量 |
| NATS JetStream 0.x API 变动 | 低 | 中 | 锁版本；升级时拆分 CI 测试 |
| 通话 Saga 补偿引入新 bug | 中 | 中 | 系统地对账循环兜底；补偿操作单独单元测试 |
| 缓存失效广播丢消息 | 低 | 低 | TTL 保底（即使 Redis Pub/Sub 丢消息，60s 后自动过期） |
| 搜索降级后用户感知搜索变差 | 中 | 低 | 降级标记告诉用户"索引同步中"；FTS 仍正常工作 |

### 5.4 关键交付物定义

每个阶段需要产出的可测量成果：

- **阶段一**：`cargo test --workspace --lib` 全绿 + 安全测试覆盖 `fan_out_raw` 权限校验
- **阶段二**：端到端测试覆盖审核→删除→WS 交付的时序测试用例；通话在单节点/双节点的加入/离开流程可通过集成测试
- **阶段三**：搜索一致性 SLA（编辑后 5s 内搜索至少 FTS 正确）；推送通知在桌面活跃时减少 80%

---

## 总结

Aero IM 的架构在**吞吐和延迟**方面做出了合理的设计决策。五个分布式一致性裂缝不是设计失误，而是**边界条件未充分枚举**的结果——当跨消费者、跨存储、跨节点的复杂度叠加时，局部的合理决策在系统层面产生了非预期的交互。

**最关键的三件事（按影响排序）**：

1. **`fan_out_raw` 缺少权限校验**（方向四）——注释和实现不一致，真实安全漏洞。修复成本最低（加一个 room_id 参数 + 缓存查询），收益最高。**应立刻修复，不等阶段划分**。

2. **跨消费者因果顺序**（方向一）——最复杂的架构问题。建议分步走：先风扇前校验（方向四修复也缓解了这里的部分风险），再评估审核同步化。

3. **通话 Saga**（方向三）——风险"只有跨节点部署才会触发"，但一旦触发影响很糟糕。代码已提供了对账模式（call_bridge_supervisor），复用这个模式将成本降到很低。

**不做的**：不引入分布式事务、不引入新基础设施组件、不做全量屏障（只在必要时加）。

---

*分析基于 2026-07-12 代码库状态，参考 AGENTS.md、五方向分析文档及其代码验证结论。*
