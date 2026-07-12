现在我已掌握足够的代码库上下文，以下是我的架构分析。我已验证了审阅文档中的每一个论断。

---

# 架构分析：Aero IM 战略深度评估

## 1. 架构评估

### 优势：经过实战检验的事件驱动根基

该系统的核心架构决策——**跨实例的 NATS JetStream 事件总线 + 进程内 Hub 扇出**——是正确的，且经过充分验证。这种 DAG 风格的拓扑结构如下：

```
生产端 → NATS subject → 持久/临时消费者 → Hub::fan_out_raw → WebSocket mpsc
```

此拓扑提供了四个重要属性：

1. **水平可扩展性**：每个进程独立消耗总线，Hub 是本地 bounded mpsc。无需代理即可扩展 WebSocket 连接。
2. **at-least-once 交付**：持久消费者 `aero-server` 从已提交的光标处恢复，即使崩溃也不会丢失房间事件。
3. **关注点分离**：`aero-im-core` 中的消息热路径从不接触推送/展开/审核逻辑——这些都是通过单独的总线消费者（`push_bot`、`moderation_bot`、`ooo_bot`）带外处理的，具有各自独立的背压域。
4. **Seq-stamped 事件排序**：已解决跨实例去重/排序的基本问题——`per-subject seq`（ROADMAP 第三版 方向一）给予每个事件一个单调标识符。

### 核心架构债务

#### 债务 1：缺乏跨节点缓存失效（P0 — 安全关键）

这是文档中"方向四"的核心主张，经确认准确。两个主要缓存——`ParticipantCache`（`participant_cache.rs`）和 `RoomMemberCache`（`room_member_cache.rs`）——各自有 **60 秒 TTL**，且**仅本地失效**。没有跨节点总线来广播"此工作区的参与者资料已更改"或"此房间的成员资格已更改"。

**安全影响**：`assert_room_access`（`im-core/service/room.rs`）通过 `room_member_cache` 查找。当管理员踢出一个用户时，被踢用户在另一个节点上仍可发送消息长达 60 秒。SLA 声明了这一点，但这对任何企业部署来说都是不可接受的。

**用户可见影响**：`Bot` 或 `Agent` 在 Node A 上更新其头像/名称；Node B 在 60 秒内提供过时的数据。

#### 债务 2：发送端非幂等 + 无 Nonce 的前端（P0 — 可复现数据损坏）

我在前端验证了 `findPendingMatch`（`web/app.js` 第 471-483 行）。它使用 `textOf()` 匹配纯文本块 + 15 秒的时间窗口。这存在一个可复现的错误：如果在 15 秒内发送两条纯文本相同的消息，第二条的服务器确认会错误地匹配第一条的待发送 DOM 节点。第二条消息就会变成**幽灵消息**——已提交到历史记录，但屏幕上只显示一条。

更根本的问题是，缺少 `send_message` nonce 意味着**待发送匹配**本质上是模糊的。当前的 15 秒窗口依赖于客户端时间戳（`Date.parse`），这在时钟偏移或闰秒修正的浏览器上是不可靠的。

#### 债务 3：媒体面容错性差（P1 — 直播韧性）

`SfuPeer`（`peer.rs` 中的 `poll()`）在底层 str0m 轮询失败时无条件返回——没有 ICE 重启、没有 DTLS 重连、没有 exponential backoff。`HlsWriter` 写入原始 TS 包而不检查 `TS_SYNC_BYTE`（`0x47`）——推流中断会产生损坏的 `.ts` 文件，且无完整性校验。`finish()` 不清理残留文件。

修复方向是正确的：流看门狗 + 媒体管道完整性检查 + `HlsWriter::push_segment()` 处的 TS 同步验证。

#### 债务 4：SeqProvider 默认本地（P1 — 多实例正确性）

`LocalSeqProvider`（`aero-im-core/src/seq.rs` 第 50 行）是 `ImService::new` 的默认值。`RedisSeqStore` 已实现（第 76 行），但并非默认值。这意味着重启后 seq 会归零——旧消息在重放后会被重新赋予 seq = 1、2、3……这违反了 seq 用于去重的契约（客户端在重启后看到重复的 seq 值）。

## 2. 扩展方向

### 方向 A：发送端 Nonce 化 & 幂等性收口

**为什么需要**：前端幽灵消息 bug（可复现，影响所有用户）+ 缺乏重试安全保证。

**核心挑战**：非向后兼容。旧的 `send_message` 帧没有 `nonce` 字段。服务器必须接受新旧两种格式，并在旧格式上退回到 `textOf` 匹配。

**预期的架构变更**：

```
WS send_message 帧
  ├─ nonce: string (新增，客户端 Ulid)
  ├─ 服务器去重键: (sender_id, nonce) → ON CONFLICT DO NOTHING
  └─ 服务器响应: { type: "confirmed", nonce, message_id }
```

客户端实现一个**指数退避重试循环**（max 3 次，加 jitter），重试相同 nonce。`findPendingMatch` 完全按 nonce 匹配，而不是按文本内容。

前端迁移计划：分两个阶段推出——首先接受服务器端的 nonce（旧版按文本回退），然后添加客户端 nonce 生成，接下来删除旧版匹配逻辑。

**对现有系统的影响**：低侵入性。这仅影响 `im-core/service/messages.rs` 的发送路径和 `web/app.js` 的待发送管理。无 DB 迁移。

### 方向 B：Web Push（VAPID / Service Worker）

**为什么需要**：最佳性价比功能。移动端推送基础设施（`PushGateway` trait、FCM/APNs 实现、`push_bot`、token 注册 API）均已完成。Web Push 是唯一缺失的平台——它是**一个 trait 实现 + 一个 JS 钩子**。

**核心挑战**：VAPID 密钥对（服务端）+ Service Worker 注册（客户端）。Service Worker 还有一个额外的问题，即需要在应用 JS 更新时使其保持最新（旧 SW 会导致推送事件丢失）。

**预期的架构变更**：

```
Tree:
  aero-push/
    ├── src/
    │   ├── lib.rs       ← PushGateway trait（已存在）
    │   ├── fcm.rs       ← FCM 实现（已存在）
    │   ├── apns.rs      ← APNs 实现（已存在）
    │   ├── web_push.rs  ← 新增 WebPushGateway
    │   └── ...          ← FakeGateway（已存在）
  web/
    ├── sw.js            ← 新增 Service Worker（仅推送处理，无缓存）
    ├── app.js           ← registerServiceWorker() + 向服务器注册端点

server config:
  AERO_PUSH_VAPID_PUBLIC_KEY / AERO_PUSH_VAPID_PRIVATE_KEY（新增）
```

**对现有系统的影响**：几乎为零。`push_bot.rs` 的运行时路由逻辑已经有一个 `PushGateways::for_platform()` 分发点。添加 `'web'` 平台分支即可。

### 方向 C：媒体面韧性（看门狗 + 完整性校验 + ICE 重启）

**为什么需要**：流中断当前不会自动恢复。推流中断会产生损坏的 TS。SRT pump 在 socket 读取错误时终止，且不尝试重连。

**核心挑战**：ICE 重启不是简单的——它需要在 SFU 重新协商 SDP。看门狗必须安全地区分"流数据间预期间隙"和"死流"。

**预期的架构变更**：

```
新增:
  live-watchdog/
    ├── StreamWatchdog         ← 60 秒 tick 扫描，检测过时的推流
    ├── AutoRecoveryPolicy     ← ICE restart / pump restart / publish fallback
    └── TsIntegrityChecker     ← push_segment() 时验证 0x47

修改:
  aero-live-webrtc/src/peer.rs
    └── SfuPeer::poll()        ← 新增错误处理路径，尝试 ICE restart 或触发看门狗

  aero-live-hls/src/lib.rs
    └── HlsWriter::push_segment()
         ├── TS 同步字节校验
         └── finish() 时清理残留段
```

**关键不变量**：看门狗必须在**每个推流提交**周期触发，而不是每个时钟周期——使用 `Instant` + bounded jitter（`MissedTickBehavior::Skip`），以避免在时钟恢复时批量误报。

**对现有系统的影响**：中等。`HlsWriter` 更改是纯增量的。看门狗是一个新的后台任务（boot 时 `tokio::spawn`）。不建议更改 SfuPeer 轮询循环——看门狗应作为独立的监督者运行，在检测到僵尸推流时重新初始化流。

### 方向 D：跨节点缓存一致性总线

**为什么需要**：安全关键的 2FA 和踢出守卫目前的陈旧窗口为 60 秒。这对于多节点部署中的合规性是不可接受的。

**核心挑战**：缓存失效总线必须与那些偏序 Nats 消息有相同的可靠性——但不是 at-least-once。失效是**幂等**的（"使键 K 过期"重复应用无问题）且**不需要排序**（"踢出用户 X"可以覆盖"用户 X 添加"——如果顺序颠倒了也没关系）。

**预期的架构变更**：

```
新增 NATS subject:
  cache.invalidate.room.{id}      ← 会员变更时发布
  cache.invalidate.participant.{id} ← 资料变更时发布

在 participant_cache 和 room_member_cache 中（→bus listener）：
    订阅 cache.invalidate.*（临时 consumer）
    invalidate() 回调时：
      删除 DashMap 条目 → 下一次 get_or_fetch 返回 DB

不变：保证 DB 读取始终通过 pub-fetch 路径
——不失效是正确性下降，不是损坏。
```

**替代方案**：跳过总线，使用 **Redis pub/sub** 进行缓存失效——轻量级、无状态、零配置。NATs 更可靠，但缓存失效不需要可靠——如果一条失效消息丢失，最多会多服务 60 秒的陈旧数据。

**对现有系统的影响**：低。仅向两个 cache 模块添加失效订阅。总线消费者是每个进程一个临时订阅——无重投，无持久状态。

### 方向 E：SeqProvider 默认集群化 + 分布式 ID 生成

**为什么需要**：`LocalSeqProvider` 是默认值，意味着多实例部署默认**存在 seq 冲突**。恢复场景会损坏实时排序。

**预期的架构变更**：

```
缓存 -> 生产者模式：
  检测 Redis → 使用 SeqStore
  无 Redis → 回退 LocalSeqProvider
  （纯启动场景）

Redis 回退行为：
  SeqStore::next() 错误 → 回退本地
  警告日志 + 使用本地生成的 seq（可能有冲突）
  当 Redis 恢复时，seq 会被下一个 get_or_fetch 纠正
```

**对现有系统的影响**：很小。`SeqProvider` trait（`aero-im-core/src/seq.rs`）已经存在且是对象安全的。只需将默认值从 `LocalSeqProvider::new()` 更改为 `RedisSeqStore`（`bin/aero-server.rs` 已经通过 `ImService::with_seq` 支持）。`ImService::new` 应保持 `LocalSeqProvider` 用于测试。

## 3. 接口设计建议

### Sequence Provider Trait（已验证良好）

```rust
#[async_trait]
pub trait SeqProvider: Send + Sync + 'static {
    async fn next_seq(&self, subject: &str) -> Option<u64>;
}
```

这个 trait 之所以正确，是因为：
- **返回 `Option<u64>`** — 无法获取序列不会阻塞投递。
- **Subjects 作为 key** — 每 subject 单调性允许多流独立分配。
- **无 `deny_unknown_fields`** — serde 忽略未知字段，以便向事件 JSON 添加序列键不会破坏消费者。

无需更改。

### PushGateway Trait（已验证准备就绪）

```rust
#[async_trait]
pub trait PushGateway: Send + Sync {
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<(), PushError>;
}
```

这就是应该的样子。然而，**对于 Web Push，我需要一个新的区别**：`PushPayload` 目前不包含 `vapid_public_key` 或 `endpoint` URL。Web Push 需要注册端点的 URL 和 VAPID 公钥。`PushPayload` 必须扩展到包含一个 `web_push: Option<WebPushPayload>` 字段——或者更好，一个泛型 `gateway_data: HashMap<String, Value>` 扩展点，以避免为每个新平台修改核心结构。

### 缓存 Trait（目前没有 Trait）

这两个缓存的公共接口目前是具体的 `struct` 方法：

```rust
impl ParticipantCache {
    fn get_or_fetch(pid, repo) -> Result<Arc<Participant>, Error>;
    fn invalidate(pid);
}
```

目前没有 `CacheInvalidator` trait。也不应该有——失效的逻辑足够简单，可以直接在 DashMap 上编码。然而，**失效订阅应该共享一个接口**：

```rust
#[async_trait]
trait CacheSubscriber: Send + Sync {
    async fn run(&self, bus: Arc<dyn EventBus>);
}
```

`ParticipantCache` 和 `RoomMemberCache` 各自实现它，以配置自己的失效回调。

### 后端兼容性

对于 nonce：不要改变 `send_message` WS 帧结构——添加一个 `nonce?: string` 可选字段。当 nonce 存在时，服务器使用它进行去重。当 nonce 不存在时（旧客户端），回退到当前基于文本的匹配。宽限期内两种格式共存。

## 4. 技术选型

### 新增依赖评估

#### Web Push：`web-push` crate

- 高质量、纯 Rust、社区维护
- 封装 VAPID + Google FCM / Mozilla AutoPush
- 核心功能约 300 行（生成 VAPID 头、加密有效载荷、POST 到端点）
- **风险评估**：低。它是一个 HTTP POST 包装器——没有 crypto 的复杂性以外的安全敏感代码（RFC 8291 使用 ECDH）。即使有 bug 也是测试捕获的，不是安全灾难。
- **替代方案**：自己实现 VAPID header + HTTP POST——没必要，crate 已验证。

#### ICE 重启：str0m 原生支持

str0m 0.19 已经有：
- `Rtc::sdp_api()` → `create_offer()` → `accept_answer()`
- ICE 重启是 SDP 级别：需要调用 `create_offer()` 并更新 ICE 凭据/候选者。

需要零外部 crate——只需正确的 str0m API 序列。无需新增依赖。

#### TS 同步字节校验：空 crate 成本

这是对 `HlsWriter::push_segment()` 的约 10 行补充。零新依赖。

### 自建 vs 采购

| 组件 | 建议 | 原因 |
|--------|----------|------|
| Web Push 交付 | **采购** `web-push` crate | 这是跨通知平台共享功能的管道。移植 VAPID 加密是错失的机会成本。|
| 流看门狗 | **自建** | 核心业务逻辑——轻量级（1 个 tick 扫描，1 个 `Instant` 字段）且领域特定。外面没有通用的"看门狗" crate 能理解"流健康"的含义。|
| 缓存失效总线 | **自建** | 这是一个整合模式，不是库。两个调用站点（参与者 + 房间成员）和一个 NATS pub 路径。一个共享 trait 来接口化失效订阅者，但实现只需要 50 行代码。|

## 5. 实施路线图

### P0（关键修复 - 当周）

| 任务 | 方向 | 预计工期 | 风险 |
|------|----------|----------|----------|
| Nonce 化 `send_message` | 方向一 | 2-3 天 | 中。需要协调服务器 + 前端变更。旧客户端有宽限期——需要两种格式。 |
| 修复 `findPendingMatch` → nonce-only | 方向一 | 1 天 | 无。纯 JS，无后端影响。 |
| Redis seq 成为 `ImService::new` 默认值 | 方向四 | 0.5 天 | 低。`ImService::with_seq` 已经开启。默认构造函数切换。 |

**P0 风险缓解**：nonce 分两阶段推出（先接受 nonce，然后要求 nonce）可防止旧客户端出现破坏性回归。15 秒的 `Date.parse` 窗口意味着在迁移期间，旧客户端在发送后至少有 15 秒的时间完成——这很具包容性。

### P1（安全与媒体韧性 - 下一轮）

| 任务 | 方向 | 预计工期 | 风险 |
|------|----------|----------|----------|
| 跨节点缓存失效（Redis pub/sub） | 方向四 | 2-3 天 | 中。失效订阅必须正确处理启动/关闭——如果订阅在缓存填充后出现，最初会有 60 秒的空窗期。通过失效订阅准备就绪后**立即**执行首次全量失效来缓解。 |
| 流看门狗 + TS 完整性检查 | 方向三 | 3-4 天 | 中。看门狗必须安全区分"流数据间预期间隙"和"死流"。使用 `room_member_cache` 模式作为模板。 |
| `SfuPeer::poll()` 错误 → ICE 重启尝试 | 方向三 | 1-2 天 | 中。str0m 的 ICE 重启 API 是低级别的——`create_offer` 参数需要研究。可能需要在测试中将 str0m 的 `Rtc` 提取到 mock 层。 |
| HlsWriter 清理 + 完成时 TS 同步字节检查 | 方向三 | 1 天 | 低。纯增量——不改变现有语义。 |

**P1 风险缓解**：看门狗从**共识驱动的扫描**开始——不仅仅是单个节点声明流死亡——以避免因瞬时网络故障而误杀流。首选模式：两个连续的看门狗 tick（120 秒超时 window）没有推流端活动 → 触发恢复。

### P2（UX 一致性 - 下一轮 + 1）

| 任务 | 方向 | 预计工期 | 风险 |
|------|----------|----------|----------|
| Web Push 实现 | 方向二 | 2-3 天 | 低。隔离最佳——零后端影响。Service Worker 注册是一次性的 JS 更改。 |
| Pending 消息超时 UI（变灰 + "发送失败？"） | 方向一 | 0.5 天 | 无。纯 UI——`findPendingMatch` 在确认到达后已经更新 DOM；超时是一个额外的渲染分支。 |
| SRT pump 重连回路 | 方向三 | 2-3 天 | 低。`pump.rs` 中的 `break` 变成 `sleep(1s)` + `continue`。确保 socket 绑定逻辑可以重入。 |
| 尝试 ICE 重连的看门狗 | 方向三 | 2-3 天 | 中。与方向三重叠。ICE 重启路径必须通过 str0m 的 SDP 重新协商 API 拆分。 |

### 总计工期

- **P0**：4-5 天（关键安全 + 可复现错误）
- **P1**：7-10 天（媒体韧性 + 节点间一致性）
- **P2**：7-9 天（Web Push + 所有方面 UX 收口）
- **总计**：约 20 个工作日用于全部四个战略方向

### 成功标准

1. **Nonce 化**：发送两条纯文本相同的消息，两条都出现在历史记录中，且前端显示两条。
2. **缓存失效**：在节点 A 上踢出用户 X，节点 B 上的 `assert_room_access` 在 1 秒内（最坏情况消息延迟 + pub/sub 传播）拒绝他们的 WebSocket 帧。
3. **流看门狗**：停止流 → 60 秒 → 看门狗触发恢复 → 新 HLS 会话从关键帧边界开始。
4. **Web Push**：在 Chrome 中登录 → 允许通知 → 最小化浏览器 → 其他用户发送消息 → 收到系统通知。点击打开正确聊天。

---

## 结论

用户提供的分析文档在四个方向上都是准确的，并得到底层代码证据的支持。文档中的不准确之处（`findPendingMatch` 使用 `textOf()` 而非 `JSON.stringify`，以及客户端 15 秒窗口的存在）已被确认，但**不影响建议的修复方案**——nonce 仍然是最正确的修复方案，因为它使匹配与文本内容完全无关，并消除了时间窗口。

项目具有罕见的质量组合：事件的**架构管道**是坚实且经过测试的（NATS + Hub + WS），但**边缘情况未完善**——确切地说，它是构建额外韧性所需的杠杆点。建议的四个方向互为补充。
