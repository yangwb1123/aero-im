好，我已完整阅读了两份文档。以下是我的架构分析。

---

# 架构分析：Aero IM 高价值扩展方向

## 1. 架构评估

### 1.1 当前架构的核心优势

**事件驱动骨架是正确选择**。系统采用 NATS JetStream 作为跨实例事件主干，配合进程内 Hub 做本地扇出，这个架构模式在 IM/直播场景下经得起验证。它的优势有三：

- **解耦彻底**：业务层（`ImService`、直播 ingest）只产事件，不关心消费方。NATS subject 空间（`im.room.*`、`live.stream.*`）天然支持按房间/按流隔离，为未来分区预留了空间。
- **水平扩展路径清晰**：`Hub::fan_out_raw` 是进程内 bounded mpsc，不依赖共享状态。多开实例只需增加 NATS consumer，无需改 Hub 逻辑。这是 PG/Redis 作为集群级事实源所不能直接提供的扩展性。
- **异构 consumer 共存**：同一个 `im.room.*` subject 上挂载了 durable consumer（`aero-server` — 做扇出）、ephemeral consumer（`run_live_bus_listener`）、queue-group consumer（`aero-bot`、`aero-push`、`aero-moderation`、`aero-unfurl`、`aero-transcribe`、`aero-ooo`、`aero-golive`）。这种"一 subject 多 consumer 语义"的生产级基建思维是正确的——虽然不是所有 bot 都需要 queue-group 去重，但主体结构健康。

**"进程内存 + Redis + PG"三层状态模型是务实的取舍**。进程内 DashMap 提供亚微秒级热路径访问（presence、member cache），Redis sorted-set 提供集群级一致的状态（viewer counts、call roster），PG 提供事实源。这三层不在同一一致性模型上竞争——Redis 做最终一致（TTL-driven），PG 做强一致（事务化）。这个分层思路正确。

**Crate 划分遵循领域边界**。`aero-im-core` / `aero-live-webrtc` / `aero-bus` 等 crate 按功能领域而非层级（controller/service/repo）划分，与 Rust 的编译单元边界一致。这避免了 Node.js 式 `src/{controllers,services,repositories}` 目录分层的阻抗不匹配——编译时强制依赖方向，无需架构守护者人工审查。

### 1.2 架构债务与局限性

**架构债务 1：缓存体系无层次、无策略。** 这是整个系统最显著的技术债。`participant_cache`（DashMap + TTL）、`room_member_cache`（纯 DashMap，无 TTL）、`Hub::room_members`（纯进程内存，无 TTL）、Redis presence（有 TTL 但仅做 presence）——四套缓存各自独立，无统一失效机制，无 L1→L2→DB 阶梯，无 write-through/refresh-ahead 语义。当房间数 >5K、在线用户 >20K 时，N 个实例的成员列表查询将对 PG 产生 N 倍放大。更关键的是，这个放大效应**不能通过加实例解决**——每新增一个实例，重复查询量也线性增加。

**架构债务 2：NATS 是唯一的单点故障。** PG 有流复制、Redis 有哨兵/集群、应用层可以多实例——但 NATS 仅一个节点，stream replicas=1。这不是运维疏忽，而是架构层面的单点：事件主干挂了，所有实时功能（消息收/发、直播弹幕、通话信令、cache-bust 广播、bot 触发）全部不可用。系统的可靠度是各组件可靠度的乘积——NATS 单点意味着全系统 MTBF 受限于单台 NATS 服务器的 MTBF。

**架构债务 3：at-least-once 投递语义下的幂等性覆盖不全。** AGENTS.md §4.2 明确指出"付费/外发/计费副作用须有幂等键"，但 bot 中仅 `ooo_bot` 有 `ON CONFLICT DO NOTHING` 幂等保护，`golive_bot` 明确标注"非幂等（重投插重复行）"，`push_bot` 标注"非事件幂等（重投会重推）"。NATS durable consumer 保证 at-least-once——宕机重启后必然重放未 ack 的事件。这意味着所有 bot 在处理**实际导致持久化副作用的路径**时必须幂等。当前不是。

**架构债务 4：`run_live_bus_listener` 使用 ephemeral consumer 但消费语义未定义清楚。** 文档说"每实例都须见全部事件给本地 watcher 扇出，丢几条弹幕无碍"——这个"丢几条无碍"的前提是消费者重启时游标重置到最新位置。但如果实例短暂断开后重连，中间丢失的事件包括：礼物打赏、点名、预测结算。这些不是"丢几条弹幕无碍"的范畴。当前没有区分弹幕（可丢失）和关键直播事件（应 durable）的 subject 划分。

**架构债务 5：Call bridge 的 PULL/PUSH 双方向数据面未接线。** `call_bridge_supervisor` 的单测覆盖了确保跨节点桥接的逻辑，但 `sfu_media.rs` 的生产 `run` 循环未被实例化。这不是"完成但未部署"而是"结构完整但集成 seam"——两端（`SfuMediaSession` 产 RTP、`CallEgress` 吞 RTP）各自有单元测试覆盖，但未组合。这种集成 seam 如果长期不接线，会逐渐偏离当前代码（如 `SfuForwarder` 接口变更而 `CallEgress` 未更新，最后两边的类型签名对不上）。

**局限 1：全系统仅一种一致性模型——强一致（事务化） + 最终一致（TTL）。** 缺少因果一致性或可线性化（linearizability）的中间选项。这在"已读回执"和"消息排序"场景中体现：同一次通话中，A 说"我们见个面"然后 B 说"好的"，如果 B 的消息因为 NATS re-delivery 排在 A 之前到达客户端，用户将看到"好的"然后"我们见个面"。当前客户端靠 per-subject seq 去重/排序——但 seq 是 per-subject 单调递增，跨 subject 不保证。这是当前设计选择（最终一致）的固有局限，不是 bug，但面对合规审计（"消息顺序可证明"）时可能不满足。

**局限 2：无写请求的幂等去重层。** 客户端发送消息时，如果 WS 断连重发，服务端可能产生重复消息（当前没有客户端 idempotency key）。`Bus::publish_room_event` 没有去重——重投就多发。对于 `Message` 类型，重复消息会在房间内出现两次（客户端侧靠 seq 去重，但 UI 上会闪烁式消失）。对于 `Reaction`、`DeleteMessage`、`EditMessage` 等类型，重复执行要么是幂等的（软删除是幂等的），要么不是。

---

## 2. 扩展方向

以下 5 个方向基于分析文档中的发现，但不重复文档内容——我从架构角度给出补充分析和独立判断。

### 方向 1：缓存体系的形式化——从 ad-hoc 到二层阶梯

**为什么需要**：不仅是性能问题，更是**架构可预测性**问题。当前缓存模式是"想加就加一个 DashMap"，无容量规划、无命中率目标、无降级策略。这不是工程管理的缺失，而是缺少承载缓存策略的抽象层。当每个开发者遇到热路径性能问题时，自行加 DashMap 而不加 TTL 或失效逻辑，会逐渐腐蚀系统的一致性假设。

**核心挑战**：

- **失效传播的原子性**：修改 room name 的操作需要：PG 事务提交 → NATS `im.room.{id}` 发 cache-bust event → N 个实例各自 invalidate L1。这"提交后广播"的模式下，如果 NATS 发布成功但本地 L1 失效失败（panic/OOM），房间名在失效前不一致。需要定义：允许不一致的时间窗口（最终一致）还是要求写后读一致性（需要等待 ack）。
- **L1 容量边界**：DashMap 默认无限增长。如果 room member 列表缓存了 10K 房间 × 平均 200 成员 × 每个 Pid 约 32 bytes ≈ 64 MB——看似不大，但如果每个 room 还缓存了 metadata、role map，且在 50 个实例上各重复一次，总 L1 消耗就是 3.2 GB 进程内存。必须硬限制 + LRU 驱逐。
- **Thundering herd 防护**：30 个实例同时重启，都对同一个热门房间的 room_members 发起 PG 查询。需要 `SingleFlight`（去重并发回填） + 冷却期的渐进式填充。

**架构变更**：

```
现状: 每模块自管缓存（participant_cache / room_member_cache / Hub::room_members）
目标: CacheLayer trait { get(K) -> Result<V>, set(K, V, TTL), invalidate(K), invalidate_prefix(P) }
      L1Cache(DashMap + LRU + TTL) : CacheLayer
      L2Cache(Redis) : CacheLayer
      TieredCache(L1, L2, fallback: Fn(K)->DB) : CacheLayer  // 组合
```

这个抽象的关键不是实现（DashMap 和 Redis 操作都很直接），而是**边界约定**：

1. `CacheLayer::get` 不保证强一致——返回的可能是过时数据（stale-while-revalidate）
2. `CacheLayer::invalidate` 不保证其他实例立刻同步——最终一致窗口 ≤ L1 TTL
3. 写路径必须在 PG 事务提交后调用 `invalidate`，不在事务中调用（脏数据可能被缓存）

**对现有系统的影响**：`participant_cache` 已是独立模块，适配 `CacheLayer` trait 改动面小。`Hub::room_members` 深入扇出路径，需要保证 L1 缓存的失效不阻塞扇出（异步 invalidate）。`ImService` 中写 participant/room 的路径需要加 invalidate 调用（当前 participant 已有，room metadata 路径缺）。

### 方向 2：NATS 基础设施成熟度——主干从单点到高可用

**为什么需要**：不仅是高可用。NATS JetStream 的 replicas=1 还给运维带来两个隐性风险：

1. **JetStream Meta 损坏 = 游标丢失**。`aero-server` durable consumer 的游标只存在 NATS 内。如果 NATS server 文件系统损坏（磁盘满、意外断电），游标恢复到最新 checkpoint，所有挂起的消息被重放——bot（ooo、push、moderation）将重做已生效的副作用。
2. **Rolling restart 期间消息丢失**。单节点 NATS 重启期间（即使只 1 秒），所有 `publish` 返回 `no responders` 或超时。`aero-bus` 当前有 `retry_on_timeout`，但重试窗口内如果客户端已超时，消息被丢弃。

**核心挑战**：

- **Raft 的写入延迟放大**：replicas=3 时，每条 publish 需要在多数节点确认（2/3）。当前单节点 publish 延迟 ~100μs，三节点 Raft ~500μs-1ms。对 `live.stream.*` 弹幕场景（高吞吐、低个别价值）来说，Raft 延迟的代价是否值得？答案：不值得。正确的策略是 `im.room.*` replicas=3，`live.stream.*` replicas=1（可重现，不需要持久）。
- **Raft 节点故障后的流迁移**：NATS JetStream 不支持自动迁移流副本到新节点。如果 3 节点集群中一个节点永久故障，`im.room.*` 的 replicas=3 无法补全为 3——只能手动 meta restore 或降到 replicas=2。这意味着集群搭建时建议初始 5 节点（容忍 1 故障后仍有 3 副本）或 3 节点 + 快速替换。
- **磁盘用量预测**：`im.room.*` 的 `max_age=7d`，如果消息速率是 50 msg/s × 平均 2KB ≈ 100 KB/s ≈ 8.6 GB/天 × 7 天 ≈ 60 GB。replicas=3 则 180 GB。需要 `max_bytes` 硬限制 + 磁盘监控。

**架构变更**：

当前 `bootstrap()` 是硬编码流配置。改为：

```rust
// 启动时从 Config 读取流定义
let streams = vec![
    StreamDef { name: "im_room", subjects: vec!["im.room.*"], replicas: cfg.nats.im_room_replicas, max_age: cfg.nats.im_room_retention, ... },
    StreamDef { name: "live_stream", subjects: vec!["live.stream.*"], replicas: 1, max_age: Duration::from_secs(3600), ... },
];
for s in streams { bus.ensure_stream(s).await?; }
```

这个变更的核心不是代码量（~100 行），而是 `Config` 结构体新增 `nats` section——这意味着 figment 的环境变量前缀要新增 `AERO__NATS__*` 键名空间。需要和现有 `AERO__DATABASE__URL`、`AERO__SERVER__*` 一致的命名规范。

**跟其他方向的关系**：NATS 集群就绪后，方向一（缓存体系）的 cache-bust broadcast 有了可靠的底层通道。反过来，方向一的 `SingleFlight` 回填可以减少 NATS 重启后的 thundering herd 对 PG 的冲击。两个方向有正反馈。

### 方向 3：Web 安全纵深——从 debug-client 到防御基线

**这个方向的核心设计问题不是技术复杂度，而是**：CSP 的策略构建权应该交给谁？

**选项 A（推荐）**：服务端自动构建 CSP 基线 + CDN-detect 策略。

服务端 `serve.rs` 在启动时扫描 `web/index.html` 中的 `<script src>` 标签，自动提取 CDN 域名加入 `script-src`。这样部署者不需要知道 CDN 域名列表。如果未来添加了新的 CDN 依赖（如地图库、图表库），CSP 自动适配。

**选项 B**：运维者手动配置 `AERO_CSP_POLICY`。当前方案——但默认值是空（not set），导致 CSP 默认不开启。这是最差的默认值——安全基线应默认开启、opt-out 而非 opt-in。

**选项 C**：构建时生成 CSP + SRI。在 CI 或 Docker build 阶段，提取 CDN script hash 注入 `index.html` 的 `integrity` 属性，同时生成 `csp.json` 喂给服务端。这给构建流水线增加了步骤，但实现了 SRI 的自动维护。

**我的建议**：选项 A（自动构建） + 选项 C（hash 注入）组合。服务端负责 CSP 头的正确性（不能因为一个 CDN 不可达就阻止整个 CSP），构建期负责 SRI 的完整性（防止构建到部署之间的篡改）。

**更深的架构问题**：当前的渲染模型是"服务端发 JSON block → 客户端 render.js 拼接 DOM"。这个模型下，XSS 风险集中在 `render.js`。如果未来引入富文本编辑器（Quill/ProseMirror/Plate），渲染模型将变为"服务端发 HTML → 客户端 innerHTML"——那是完全不同的威胁模型。需要在做 CSP 前确定渲染模型是否可能变更。

**影响评估**：CSP 基线 + SRI 注入是纯头部/属性变更，对现有逻辑零影响，回退只需去掉 header。这是五个方向中**侵入最小、ROI 最高的**。

### 方向 4：异步语音/富媒体——UX 层补全

**这个方向的架构问题不是"加几个 Block variant"那么简单。** 核心问题是：**语音/视频消息的录制、上传、播放流程是否应该与服务端渲染模型耦合？**

当前模式：`Block::Voice { blob_id, transcript }`—服务端存 blob_id，客户端 fetch blob 后播放。对于语音消息，这个模式足够。但对于视频消息和对讲机模式，有两个架构岔路：

**岔路 1：服务端中转**（当前模式）。所有媒体 blob 走 `POST /api/blobs` 上传 → 服务端存 blob_store → 客户端 GET blob 播放。优点是服务端有完整的审计和存储管理。缺点是延迟（下载完成后才播放），带宽成本（服务端作为 relay）。

**岔路 2：客户端 P2P + 服务端信令**（类似 WebRTC）。对讲机模式尤其适合：发送端用 `RTCPeerConnection` 或纯 UDP 直接推流给接收端，服务端只做发现和信令。延迟可降至 200ms 以内，带宽成本为零。缺点是复杂性激增——NAT 穿透、ICE 重连、接收端离线时的消息持久化。

**我的建议**：语音/视频消息走服务端中转（岔路 1）——安全模型简单、离线可用、合规可审计。对讲机模式走 P2P + 服务端 fallback（岔路 2 与 1 的结合）——如果 P2P 建立失败，服务端做 relay。

**更关键的设计决策：waveform 数据的归属。** 反馈审稿正确地指出，波形数据需要在客户端用 `AnalyserNode` 采集。但架构层面需要决定：波形数据是**消息的固有属性**（随消息存储和传输）还是**客户端渲染的副产品**（每次播放时重算）？

- 固有属性方案：`Block::Voice { waveform: Vec<u8> }` — 发送端录完即算好波形，存 blob store，随消息广播。优点：接收端即开即播，不需重算。缺点：存储开销（每条语音消息多 ~2KB 波形数据）、发送端 CPU 开销。
- 副产品方案：客户端在播放时实时采集 waveform。优点：零额外存储。缺点：不能显示时间轴进度条（因为不知道总时长）、播放时才能看到波形。

**我的建议**：固有属性方案。2KB 的波形数据对存储影响可忽略，但 UX 提升显著（语音消息列表可预览波形，无需下载音频文件）。

### 方向 5：消息生命周期策略引擎——从定时器到规则引擎

**这个方向是我认为最有远见但也最容易做坏的方向。** 核心风险是**策略引擎变成内部复杂度黑洞**。

当前消息生命周期功能是"各扫门前雪"：sweep 定时器扫 `messages` 表、`ephemeral_sweep` 扫 `ephemeral` 表、`ban_sweep` 扫 `bans`、`points_sweep` 扫 `points`。把它们统一到一个规则引擎下，如果设计不当，会导致：

1. **策略评估成为消息发送路径的同步瓶颈。** 如果每次 `send_message` 都要查 `workspace_policies` 表再逐条评估 filter，延迟将不可接受。反馈审稿建议"启动时全量加载策略到内存"是对的——但策略变更时如何无缝热更新？用 `Arc<RwLock<PolicyEngine>>` + 版本号，还是用 NATS 广播通知所有实例 reload？

2. **策略冲突的完备性难以证明。** 当 retention=7d 和 legal-hold=6mo 冲突时，legal-hold win——这是对的。但当有 10 条策略匹配同一消息时（workspace 级全局策略 + room 级策略 + block-type 策略 + role-based 策略），优先级排序的正确性需要穷举测试。更复杂的场景："retention=7d for all messages" 与 "retention=30d for messages with @mention" 与 "retention=90d for messages in #legal channel"——优先级规则从"高优先级赢"变为"取最大值"还是"范围最窄赢"？这需要明确的冲突解决策略文档。

3. **清扫的频率不能是一刀切的。** 当前 `RETENTION_SWEEP_SECS=3600` 对所有频道一样。但 #legal 频道可能要求每小时清扫确保合规，而 #watercooler 频道每天扫一次就够。策略引擎设计时就必须支持 per-room sweep 频率，否则运营者仍需要手动调全局配置。

**架构建议**：

```
PolicyEngine {
    // 启动时加载，策略变更时通过 NATS bus 刷新
    policies: Arc<RwLock<HashMap<Option<WorkspaceId>, Vec<CompiledPolicy>>>>,
}

// 每条策略在加载时预编译 filter 条件为位掩码 + 函数指针
CompiledPolicy {
    room_matcher: fn(RoomId) -> bool,          // match room_id
    block_matcher: fn(&[Block]) -> bool,       // match block types
    pii_matcher: fn(&str) -> bool,             // simple PII pattern match
    action: PolicyAction,                      // 不包含 DB 查询
    priority: u32,
}
```

策略变更时通过 NATS 广播 `PolicyChanged { workspace_id }`，各实例在接收到后 `Arc::make_mut` 替换对应 workspace 的策略集。这样策略变更的传播延迟 = NATS 延迟 + 锁切换时间（< 100ms），不需要重启。

**影响范围**：`ImService::send_message` 和 `edit_message` 路径需在 `messages.insert` 之前插入 `policy_evaluator.evaluate(&msg)?.await_action()`。如果 action 是 `delete_after_secs`，不阻塞发送——只更新 `messages.expires_at` 字段（已有）。这意味着策略评估在关键路径上只做内存操作 + 选择性地更新字段，不引入额外事务。

---

## 3. 接口设计建议

### 3.1 缓存抽象层接口原则

定义 `CacheLayer` trait 时，关键在于**边界约定而非实现复杂度**：

```rust
trait CacheLayer<K, V> {
    /// 可能返回过时数据。调用者必须接受最终一致性。
    fn get(&self, key: &K) -> impl Future<Output = Result<Option<V>>>;
    
    /// 设置缓存条目，带 TTL。
    fn set(&self, key: K, value: V, ttl: Duration) -> impl Future<Output = Result<()>>;
    
    /// 使单个条目失效。不保证其他实例立刻同步。
    fn invalidate(&self, key: &K) -> impl Future<Output = Result<()>>;
    
    /// 使一组条目失效（prefix pattern）。
    fn invalidate_many(&self, pattern: &str) -> impl Future<Output = Result<()>>;
}
```

**原则 1**：`get` 不保证强一致——如果调用者需要写后读一致性，必须走 DB（或在 `get` 后自行校验版本号）。

**原则 2**：`invalidate` 必须是异步的，不能阻塞写路径。写路径提交 PG 事务后 `spawn` 一个 invalidate 任务。

**原则 3**：不提供 `get_or_fetch` 在 trait 层面——那是 `TieredCache` 组合器的职责。

### 3.2 NATS StreamConfig 接口

当前 `bootstrap()` 中 `get_or_create_stream` 的参数是内联 hard-coded。改为：

```rust
// aero-bus/src/jetstream.rs
pub struct StreamDefinition {
    pub name: String,
    pub subjects: Vec<String>,
    pub retention: RetentionPolicy,
    pub storage: StorageType,
    pub replicas: usize,
    pub max_age: Duration,
    pub max_bytes: i64,
    pub max_msg_size: i64,
    pub max_consumers: i32,
}

impl EventBus {
    /// 幂等服务端创建/更新流配置。仅当 replicas > 当前节点数时告警（不强退）。
    pub async fn ensure_stream(&self, def: &StreamDefinition) -> Result<()>;
}
```

这个接口的元设计原则是：**可配置但不可动态变更**。`ensure_stream` 在启动时调用一次，运行时不变更流配置。NATS JetStream 的流配置变更（如 `max_bytes` 动态调整）是可能的但写入后不可回退——最佳实践是只在部署时变更。

### 3.3 策略引擎接口

策略引擎的接口应该**薄且可组合**：

```rust
// aero-policy/src/evaluator.rs
pub struct PolicyEvalInput<'a> {
    pub room_id: RoomId,
    pub workspace_id: WorkspaceId,
    pub sender_id: ParticipantId,
    pub blocks: &'a [Block],
    pub has_pii: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>, // 当前消息的 expires_after_secs
}

pub enum PolicyDecision {
    /// 无匹配策略，沿用默认行为
    Noop,
    /// 此消息应在何时被删除（None = 永不删除）
    SetExpiration(Option<DateTime<Utc>>),
    /// 立即阻止消息发送（同步审核场景）
    Reject { reason: String },
    /// 仅记录审计，不修改行为
    AuditOnly,
}

pub trait PolicyEngine: Send + Sync {
    fn evaluate(&self, input: &PolicyEvalInput<'_>) -> PolicyDecision;
    fn reload(&self, workspace_id: WorkspaceId) -> impl Future<Output = Result<()>>;
}
```

**接口原则**：`evaluate` 是纯同步操作，不涉及 IO。所有策略预加载在内存中。`reload` 是异步操作，触发内存中策略集的替换。

### 3.4 向后兼容性策略

五个方向中，只有方向四（富媒体）需要新增 Block variant，这涉及 `RoomEvent` 的序列化格式变更。其他方向都是添加剂（新增模块/路由/配置），不修改现有消息格式。

对 Block variant 的向后兼容建议：

- **宽松的反序列化**：`#[serde(deny_unknown_fields)]` 当前在 Block 上开启？如果开启，新增字段会导致旧版本服务端拒绝反序列化新消息。建议改为默认容忍未知字段（或者至少在 consumer 端用 serde 的 `deny_unknown_fields` 关闭）。
- **新增字段用 Option + default**：`voice.duration_secs`、`voice.waveform`、`video.thumbnail_blob_id` 都用 `Option` + `#[serde(default)]`，旧客户端收到新消息时忽略这些字段。
- **RoomEvent 的 `kind` tag 问题**：新 `Block` variant（Video、Location）不和 `RoomEvent` 在一个 enum 下，所以不存在 `duplicate field kind` 问题。但需要在 `render.js` 添加对应的渲染分支——如果缺少，新 block 类型在 UI 上会显示为未知类型（或静默忽略）。

---

## 4. 技术选型

### 4.1 缓存层：是否需要引入第三方缓存库？

**选项分析**：

| 方案 | 优点 | 缺点 |
|------|------|------|
| 手写 DashMap + TTL + LRU | 零额外依赖；`participant_cache` 已存在同类实现 | 需要自行实现 LRU 驱逐（`lru` crate 或手写 `LinkedHashMap`） |
| `cached` crate | 成熟的 proc-macro 缓存（`#[cached]`） | 侵入式宏；不支持异步 `get_or_fetch` 的定制失效策略 |
| `moka` crate | 高性能并发缓存；支持 TTL + 容量 + 异步 | 新依赖；与 tokio runtime 集成需注意 |
| `quick_cache` | 轻量，纯 LRU | 无 TTL 支持；无异步 |

**我的建议**：不引入新依赖。缓存层的核心复杂度不在缓存实现（DashMap 已够用），而在**失效传播**和**层级组合**。手写 `L1Cache` 封装 DashMap + LruMap + TTL 约 200 行，复用 `participant_cache` 已有 pattern。L2 Redis 层用 fred 的现有连接池即可（`fred` 已是工作依赖）。

当 L1 命中率成为瓶颈时，才考虑 `moka`——但那是优化阶段，不是架构阶段。

### 4.2 策略引擎：是否需要规则引擎 DSL？

**选项分析**：

| 方案 | 优点 | 缺点 |
|------|------|------|
| Rust struct + 条件判断（当前建议） | 类型安全；编译期验证；零运行时开销 | 策略变更需部署；非技术人员无法编辑 |
| JSON/YAML DSL (如 `{"if":{"room":"#legal"},"then":"retain_90d"}`) | 可运行时加载；管理员 UI 可直接生成 | 反序列化错误处理复杂；条件组合的完备性难保证 |
| 外部规则引擎（如 `casbin`） | 成熟的 RBAC/ABAC 引擎 | 引入重量级依赖；与 Rust `PolicyDecision` 语义不匹配 |

**我的建议**：Phase 1-2 用 Rust struct 直接编码条件匹配。当策略数量超过 50 条或需要管理员 UI 时，考虑引入 JSON DSL——但 DSL 的 schema 应该**从 Rust enum 自动派生**（通过 serde），写死一份 JSON schema 文档。

不要引入 `casbin` 或 `drools` 之类的外部规则引擎——IM 消息策略的复杂度远低于 RBAC（角色只有 member/admin/owner 三种），不值得背负一个通用规则引擎的认知负载。

### 4.3 对讲机模式：NATS subject vs WebRTC data channel

方向四-D（对讲机模式）的架构决策：

| 方案 | 延迟 | 复杂度 | 离线能力 |
|------|------|--------|---------|
| NATS subject 中转（ephemeral） | ~5-10ms（内网 NATS） | 低——复用现有 `live.stream.*` | ❌ — 接收端离线丢消息 |
| WebRTC data channel (P2P) | ~50-100ms（ICE + DTLS） | 高——ICE/STUN/TURN + 信令 | ❌ 同左 |
| NATS JetStream（durable） | ~10-20ms | 低——但存储开销高 | ✅ — 可回放 |

**我的建议**：Phase 1 用 NATS `live.stream.*` ephemeral subject + 服务端 relay。这不引入新依赖（NATS 已存在），无 ICE 复杂性。Phase 2 如果低延迟成为瓶颈（实测 >200ms），再引入 WebRTC data channel。

**不推荐一开始就走 P2P**，因为对讲机的"即时感"核心指标是**首帧延迟**而非**端到端延迟**——NATS 内网往返 ~5ms 对听感无影响，但 ICE 连接建立 ~500ms 对听感影响显著。

### 4.4 评估标准总结

| 考核维度 | 通过条件 |
|---------|---------|
| 已于 Cargo.toml 中存在 | 优先使用现有依赖（fred、tokio、serde、dashmap、async-nats） |
| 纯 Rust（无 C FFI 除系统库外） | 符合 `unsafe_code = "forbid"` 政策 |
| 与 tokio runtime 兼容 | 库是 async 或 spawn_blocking 适配 |
| MSRV ≤ 1.80 | 否则需要论证升级收益 > 成本 |
| 许可证兼容 | MIT / Apache-2.0 / BSD-2/3 |

---

## 5. 实施路线图

### 5.1 综合优先级（整合两份文档的建议）

| 方向 | 优先级 | 排序理由 |
|------|--------|---------|
| **二 · NATS 基础设施** | **P0** | 单点故障——全系统实时功能依赖 NATS。PG 和 Redis 各有 HA，NATS 唯一的单点。故障影响面最大。 | 
| **一 · 多级缓存（Phase 1）** | P0 | 当前缓存 ad-hoc 模式的性能瓶颈在 5K 房间 / 20K 在线用户时触发——但系统可能已达此规模。L1 room metadata 缓存工作量仅 ~1w，但能消除最具放大效应的重复查询。 |
| **三 · Web 安全基线** | P1 | 低成本（~3d）高合规价值。产品定义为"企业级 IM"而 CSP 默认关闭——这是最容易被安全审计质疑的缺口。 |
| **一 · 多级缓存（Phase 2: L2 Redis）** | P1 | 依赖 NATS 集群就绪（cache-bust 广播通道需要可靠底层）。Phase 2 可以等 Phase 1 上线后观察命中率再决定是否实施。 |
| **四 · 富媒体（位置 + 语音播放器）** | P2 | 产品差异化价值高，但非基础设施依赖。语音播放器依赖 `render.js` 改造，与服务端无关——可以纯前端团队独立推进。 |
| **五 · 策略引擎（Phase 1）** | P2 | 合规价值明确但非紧急。当前 `sweep` + `legal_hold` 的组合已在生产运行。策略引擎的复杂性需要充分设计期。 |
| **四 · 富媒体（视频/屏幕/对讲机）** | P3 | 侵入最大（新增 Block variant + 全新 WS 帧），且对讲机模式依赖 NATS 集群就绪（低延迟保证）。 |
| **五 · 策略引擎（Phase 2+ 管理员 UI）** | P3 | 需要前端投入。在策略引擎 API 稳定前不应启动。 |

**执行顺序**：

```
Phase 1 (Month 1)
├── NATS Phase 1-2 (集群 + ensure_stream + 消息大小门控)      — ~6d
├── 缓存 Phase 1 (room metadata L1 + 写透失效 + 预热)         — ~1w
└── Web 安全基线 (CSP 默认开启 + SRI + render.js 审计)       — ~3d

Phase 2 (Month 2)
├── NATS Phase 3-5 (Prometheus 指标 + 游标备份 + 文档)       — ~3d
├── 缓存 Phase 2 (L2 Redis + 阶梯 + SingleFlight)            — ~2w
├── 位置分享 (纯 Block::Location + 地图渲染)                — ~2d
└── 语音消息播放器 (波形 + 变速 + 进度条)                    — ~6d

Phase 3 (Month 3-4)
├── 策略引擎 Phase 1-3 (CRUD + 评估 + 消息路径接入)         — ~2.5w
├── 视频消息 + 屏幕录制                                      — ~2w
└── 策略引擎 Phase 4-5 (清扫接入 + 审计日志)                — ~1w

Phase 4 (Month 4-5)
├── 对讲机模式                                               — ~1.5w
├── 策略引擎管理 UI (REST 已有，补前端)                     — ~1w
└── 富媒体收尾 (CDN 缩略图缓存、转码管线)                   — ~1w
```

### 5.2 阶段性里程碑

| 里程碑 | 时间点 | 可验收结果 |
|--------|--------|-----------|
| M1: NATS 集群化 | Month 1 Wk1 | `docker-compose.yml` 有 3 节点 NATS cluster；`im.room.*` replicas=3；kill 任一 NATS 节点，消息零丢失 |
| M2: 缓存基线上线 | Month 1 Wk3 | room metadata L1 缓存命中率 > 70%（生产验证）；写路径 invalidate 覆盖 room metadata + member list；NATS cache-bust 广播工作 |
| M3: Web 安全 A+ | Month 1 Wk4 | `securityheaders.com` 评级 A+；SRI 在 CI 中自动注入验证；render.js 零 `innerHTML` 执行 |
| M4: L2 Redis 缓存 | Month 2 Wk3 | L1+L2 综合命中率 > 95%；重启后 30s 内预热完成；`cache_hit_ratio` Prometheus 仪表盘 |
| M5: 语音可播放 | Month 2 Wk4 | `Block::Voice` 有内联播放器 + 波形 + 变速；transcribe_bot 转录文字在播放器下方显示 |
| M6: 策略引擎可用 | Month 3 Wk4 | 运营者可通过 REST API 创建 retention/legal-hold 策略；消息发送受策略约束；`audit_events` 记录策略执行 |
| M7: 对讲机上线 | Month 4 Wk2 | `<2s` 首帧延迟对讲消息；走 `live.stream.*` 扇出；接收端自动播放（非静音需用户交互后激活） |
| M8: 全线富媒体 | Month 4 Wk4 | 视频消息可录制+上传+播放；屏幕录制可用；位置分享可渲染地图；全部 Block 有对应 UI 渲染分支 |

### 5.3 风险和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| NATS Raft 集群运维复杂度超预期（快照大小、磁盘写满、leader 选举风暴） | 中 | 高 — 集群不可用 | 先 staging 运行 2 周收集指标后再上生产；`max_bytes` + `max_age` 双限制；初始 5 节点非 3 节点 |
| 缓存 L1 失效广播的 NATS 延迟导致不一致窗口 > 60s（用户看到已删除的成员还在列表里） | 低 | 中 — UX 短暂不一致 | 已有 TTL 作为兜底；不一致不影响数据安全（只影响视图）；可记录 `cache_inconsistency` 指标 |
| Phase 4 对讲机模式性能不达标（NATS ephemeral consumer 首帧延迟 > 500ms） | 中 | 高 — 功能不可用 | Phase 1 不对延迟做硬承诺；如果 NATS 不够快，后移到 Phase 2 用 WebRTC data channel |
| 策略引擎 Phase 1 上线后，运维者创建了冲突策略导致消息被错误删除 | 低 | 高 — 数据丢失 | `DELETE FROM messages` 前写 `audit_events` 记录，且清扫是批量非逐条，有 1h 窗口可回滚；优先实现 `legal_hold` 守卫（硬保护） |
| 方向一与方向二的开发者同时修改 aero-bus crate（一个加缓存 trait，一个改 jetstream.rs） | 中 | 低 — 合并冲突 | 两个方向在不同的 crate 或不同文件内工作（缓存走 `aero-cache/` 或 `aero-common/src/cache.rs`，NATS 改在 `aero-bus/src/`）不会冲突 |
| 语音波形采集在 iOS Safari 上不支持 `AudioContext` | 高 | 中 — iOS 用户无法使用波形 | 降级方案：无波形显示，仍可播放音频；前端特征检测 `typeof AudioContext !== 'undefined'` |

### 5.4 最大不确定因素

方向二（NATS 集群）最不确定的风险不是技术性而是**运维性**：当前团队是否具备 NATS JetStream Raft 集群的运维经验？如果否，建议在 Phase 1 实施前先搭建一个 3 节点 staging 环境运转 2 周，观察 Raft snapshot 行为、磁盘增长速率、leader 选举对 publish 延迟的影响。在集群验证通过之前，不要直接修改生产环境的 `docker-compose.yml`。

方向五（策略引擎）的最大不确定因素是**产品需求本身**。当前没有合规审计的客户案例驱动这个功能——"5 年后可能需要的合规"与"当前 3 个客户明确要求消息留存策略"之间存在差距。建议在 Phase 1 设计完成后，与潜在企业客户做一次需求验证，确认优先级排序再做开发投入。

---

## 总结

这份架构分析覆盖四个维度：**架构债务识别**（缓存 ad-hoc、NATS 单点、幂等覆盖不全、CallBridge seam）、**扩展方向纵深**（5 个方向的补充设计决策和岔路分析）、**接口原则**（边界约定优先于实现复杂度）、**实施路线**（"NATS 集群 → 缓存 Phase 1 → Web 安全基线"作为首月冲刺目标）。

核心判断：**NATS 集群化是最紧迫的架构改进。** 不是因为它的技术复杂度高（~6d 工作量），而是因为它是全系统唯一没有 HA 的组件——PG 流复制 + Redis 哨兵 + 应用多实例 vs NATS 单节点，这个不对称本身就是架构缺陷。方向一的缓存体系可以缓解 PG 压力，但无法解决事件主干单点故障——而事件主干挂了，缓存再好也没有数据可传。
