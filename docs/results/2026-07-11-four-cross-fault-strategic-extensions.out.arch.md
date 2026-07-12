现在我已对整个代码库有了扎实的了解。以下是我的架构分析。

---

# 架构分析：Aero IM 四交叉断层

## 1. 架构评估

### 当前架构优势

该系统的架构在几个重要方面表现出色：

- **事件驱动骨架**是最大优势。NATS JetStream（持久）和 ephemeral consumer 的双重模式正确地划分了耐久性预期：房间事件跨实例重投（at-least-once），直播流弹幕则按 ephemeral 模型可丢弃。这种区分成熟且务实。
- **Crate 分离**遵循了严谨的自下而上依赖图。`aero-common` 作为真正的叶子 crate（无内部依赖），`aero-bus`/`aero-storage`/`aero-auth` 处于基础层，IM 与直播领域则分别建在其上。无环形依赖——这在一个有机增长的代码库中实属不易。
- **集群状态通过 Redis sorted-set 实现**是正确的设计决策。没有使用进程内存作集群级协调（不像许多相似规模的平台那样会悄悄引入这种设计）。Presence、观看计数、通话 roster——这些通过 TTL 自然过期的方式都是经过验证的生产模式。
- **Bot 守护进程模式**（每个 bot 一个 durable NATS consumer，有独立的 consumer 名称）使得 bot 可以独立扩展、独立失败，而不会相互阻塞。`fail-open` 语义（AI 错误礼貌回应而非崩溃）、幂等守卫（被 transcribe_bot 的 `transcript.is_none()`、unfurl_bot 的 `has_metadata` 等体现）、以及有界预算（moderation_bot 的 `KeyedCostBudget`）都展示了对生产化细节的深入理解。
- **`Block` 枚举作为统一的内容模型**，在输入（用户发送）、存储（数据库 JSON）、扇出（NATS → Hub → WebSocket）和 AI 上下文窗口之间共享相同的形状——无需在各个边界进行有损转换。这是一项强有力的架构决策。

### 当前架构局限性

四道交叉断层精准地指出了痛点：

1. **Bot 生态缺乏生产护栏**：除 moderation_bot 外，**没有** bot 级别的速率限制或预算。`agent_bot` 每次 `@` 都会触发一次 AI 调用，未设上限。Bot 令牌无法吊销（无 PAT 式撤销）。Bot 操作缺乏审计追踪——没有 `audit_bot_actions` 表。Bot 编排器不存在，因此无法搭建顺序化/有状态的 bot 工作流。Webhook dispatcher 和 bot 层之间没有统一的挂起/失败/死信抽象层。

2. **媒体管道可观测性高度不均**：SRT 和 WHIP 各自拥有孤立的 metrics 模块（3-4 个局部名称），而 **SFU (`aero-live-webrtc`) 和 HLS (`aero-live-hls`) 完全没有埋点**。缺失的维度包括：端到端延迟（摄入 → 转码 → 交付）、每轨道丢包率、ICMP/连接级错误、缓冲区水位（用于反压检测）。中央 `metrics.rs` 中仅有一个媒体级计数器（`LIVE_WHIP_SESSIONS`）。没有链路追踪上下文跨越媒体管道边界。生产故障排除需依赖 `ephemeral` 消费者的丢弃行为作为隐式信号，而非显式指标——这在压力下是不安全的。

3. **数据生命周期缺少统一治理框架**：留存策略通过按工作区/频道的 `retention_days` 列实现，由 `retention_sweep` 定时器执行清扫。法务保全单独叠加在 `legal_holds` 表中。GDPR 删除通过 `delete_participant` 在多个表上进行擦除，辅以 blob GC。然而，**没有集中的 `data_class` 注解**（标记数据为“用户内容”/“系统审计”/“临时”/“计费”）来驱动保留、审查和加密策略。数据分类是跨所有数据库表的横切关注点——法律与合规要求（按数据类型定义保留期，而非按工作区）无法在不重新实现策略引擎的情况下映射到当前设计。信息屏障（`info_barriers`）存在但范围狭窄。

4. **Block 版本化缺失导致客户端兼容性风险**：`Block` 枚举使用 serde `tag = "type"`，但**没有 `deny_unknown_fields`**，也没有与客户端的版本协商机制。`ServerFrame::Welcome` 不公开 `block_caps`（客户端能力标识），因此协议演进依赖于乐观解析和静默降级。Web 端的 `default` fallback（呈现 `[${b.type}]`）虽然比崩溃更好，但无法抵御结构化破坏性变更（重命名字段、变更 payload 形状）。契约式扩展点——serde 的 `deny_unknown_fields`、`block_caps` 握手字段、`Card` payload 的 schema 注册表——完全不存在。

### 关键设计决策评估

| 决策 | 评估 |
|---|---|
| **通过 NATS 事件总线 + 进程内 Hub 扇出** | 极佳。清晰地分离了跨实例传递（NATS）和每个进程的 WebSocket 扇出（Hub）。bounded mpsc 提供了自然的背压。 |
| **`Block` 作为序列化格式 + 存储格式 + AI 上下文** | 强有力，且承担了相应的责任。无有损转换带来的好处大于耦合风险——但缺少版本协商会增加长期成本。 |
| **HashEmbedder 作为 Voyage 退化路径** | 务实。确保 AI 管道在无 API key 的开发/CI 环境中保持可测试。对齐到 1024 维保持与 voyage-3 的接口兼容性。 |
| **SFU 状态全部在进程内（Arc\<RwLock\<HashMap\>\>），无 Redis** | 对延迟有好处（无序列化开销），但排除了媒体级 HA。一个节点崩溃会丢失所有 SFU 会话——对于通话来说可以接受，但直播流会被打断。这是六期之前的决定——对于当前阶段或许可以接受，但如果直播成为生产关键负载，最终需要被重新审视。 |
| **面向定时器的架构（清扫/心跳/回填）** | 合理。`MissedTickBehavior::Skip` 和 best-effort warn 语义正确。然而，缺乏统一的定时器健康仪表板——重叠或卡住的定时器会静默失败。 |
| **迁移编译时嵌入** | 正确且安全。sqlx `migrate!("../../migrations")` 确保迁移 hash 与二进制文件一同校验。构建与迁移分离的工作流（§4.2）是正确操作的必要文档——强制执行正确顺序。 |

---

## 2. 扩展方向

### 方向 1：Bot 运行时与配额引擎（P0）

**为什么需要**：当前，`agent_bot` 每次 `@mention` 都会无限制地消费 AI 额度。生产租户需要每位用户/每工作区的配额、用于预算跟踪的池化额度、AI 调用的消耗性审计追踪、以及 bot 令牌吊销 OAuth 风格的撤销机制。

**核心挑战**：
- 在 AI 预算系统（`AiWorker` 中的 `CostBudget`，`KeyedCostBudget`）和 bot 层之间存在间接性。`agent_bot` 绕过 `ai_jobs` 队列，直接调用 `ai.answer_question()`——因此 worker 的预算执行器对其不可见。
- 配额强制执行点必须在总线消费者路径（`run_bus_listener` → bot 分发）中在首次调用 AI 之前被嵌入。在异步 bot 边界上实施配额而不阻塞消息扇出需要对配额检查进行非阻塞预授权。
- Bot 令牌撤销需要集成到现有的 PAT 撤销基础设施中（`aero-storage` 中的 `revoked_token` 表），并可能需要在 `agent_bot` 中设置一个撤销检查缓存。

**预期变更**：
- 引入 `BotRuntime` 抽象层，包裹 bot 注册、调度、配额兑现和审计日志记录。
- 将 `agent_bot`（和其他 bot）迁移为通过 `BotRuntime` 触发，而非直接来自总线消费者。
- 扩展 `ai_jobs` 系统以处理交互式（同步式）AI 请求——或者创建配额感知的 `ai.answer_question` 封装器。
- 添加 `bot_sessions` 表，映射 bot 令牌 → 权限 → 撤销状态。
- 添加 `audit_bot_actions` 审计日志表。

**对现有系统的影响**：低至中。bot 目前以独立的模块形式存在——加入编排器更多是引入包装层，而非重写。`agent_bot` 的同步 AI 路径需要最多的关注。

### 方向 2：统一媒体可观测性框架（P0）

**为什么需要**：负责媒体交付的 crate——`aero-live-webrtc`（SFU）和 `aero-live-hls`——在 Prometheus 指标方面是暗箱。生产诊断在不通过 ad-hoc 日志注入的情况下将无法诊断缓冲区膨胀、轨道级别丢包或端到端延迟。目前，工程师只能看到“连接建立”和“连接断开”——中间发生的一切都是不可见的。

**核心挑战**：
- SFU 转发循环是高度异步的（`SfuMediaSession::run`，`SfuForwarder::on_rtp`）——在每个数据包路径上添加计数器而零成本抽象需要仔细测量（Rust 令人敬畏，但热路径上的原子递增会影响缓存行）。每个数据包的指标（如 `RTP_PACKETS_RECEIVED_TOTAL`）应聚合到批量更新中，或使用 `metrics` crate 的 `Counter` 类型，该类型在内部处理缓存行乒乓。
- HLS 切片器（`HlsWriter`）运行在定时器上——段延迟指标需要跨 `FlvToTsConverter` → muxer → 磁盘写入的计时以测量端到端交付延迟。
- 跨 crate 的链路追踪需要将 `tracing` span 结构化地传播。这意味着要修改 `aero-bus` 的事件发布以携带追踪上下文——这是目前不存在的横切关注点。

**预期变更**：
- 在 `aero-common` 中定义一组**共享媒体指标名称**（当前，SRT 和 WHIP 定义各自的局部名称——这是能工作的但会成倍增加告警配置）。中央命名空间实现统一。
- 向 `aero-live-webrtc` 添加 `metrics.rs`（轨道级延迟/丢包/抖动），向 `aero-live-hls` 添加（段延迟/大小/编码时间）。
- 通过 `aero-bus` 和 NATS 消息头携带追踪上下文。
- 仪表盘就绪：为四个 crate 各提供一个 Grafana 面板原型。

**对现有系统的影响**：低。指标是附加性的，不会改变现有逻辑。命名空间合并是一次简单的重构。

### 方向 2b：SFU 状态外部化（P2，直播高可用性的前提）

**为什么需要**：当前 SFU 状态（`SfuRouter` 中的 `Arc<RwLock<HashMap>>`）是进程本地的。节点崩溃意味着所有活跃的 WebRTC 会话被终止。对于开发/演示场景可以接受，但生产直播流需要节点故障时以秒级（而非分钟级）恢复。

**核心挑战**：
- SFU 的状态本质上是有状态的且具有低延迟要求。将每个转发决策推送到 Redis 会带来数十毫秒的开销——对于 RTP 转发是不可接受的。
- 可行的折中方案：将**会话路由表**（哪个 `participant_id` → 哪个 `node_id`）存储在 Redis 中（遵循现有的 `call_route` / `stream_route` 心跳模式），同时在本地进程内保留重试缓冲。崩溃的节点上的活跃会话会丢失，但客户端可以重新连接并被路由到健康节点。
- 这意味着 `call-bridge` 也需要外部化——它目前假设节点间直接路由。

**对现有系统的影响**：高。远非小型变更。最佳时机是在 SFU 被验证为生产关键路径之后。

### 方向 3：数据分类与统一生命周期治理（P1）

**为什么需要**：当前，决定“保留某内容多久”的规则分布在工作区设置（`retention_days`）、法律表格（`legal_holds`）、以及隐式应用逻辑（`ephemeral` 消息在读取后硬删除）之间。添加 `data_class` 属性允许：按数据类型（消息 vs. 文件 vs. 呼叫记录 vs. 系统审计日志）设置保留期限；按分类（PII/敏感/公共）动态路由加密；以及架构演进期间的数据屏蔽/掩码。

**核心挑战**：
- 将 `data_class` 添加到核心表需要迁移 + 默认值 + 可空的迁移路径——幸运的是，Postgres 在添加可为空列时不会重写表。
- 策略引擎（用于评估 `data_class` → 操作）必须是声明式的，而非在存储库方法中硬编码。一种选项：在 Redis 或数据库中保留一个 `data_policy` 表，拥有（可缓存的）规则：`(data_class, action) → ttl / encryption_key_id / mask_pattern`。
- `legal_hold` 例外必须覆盖分类级策略——当前通过 `NOT EXISTS` 子查询处理，该子查询会随表增长而变慢。基于索引的查找或物化视图将是更好的选择。

**预期变更**：
- 新枚举 `DataClass { UserContent, AuditLog, Ephemeral, Billing, System }` + 迁移添加 `data_class` 列。
- 策略评估器模块（条件可缓存，DB 备份）。
- 将 `retention_sweep` 重构为查询 `data_policy` 表而非硬编码 `retention_days` 列。
- `legal_holds` 集成：保留期截止后策略必须仍应用于保留的消息——当前清扫器仅通过 `NOT EXISTS` 跳过它们。

**对现有系统的影响**：中。每次扫描需要数据分类，但 `retention_sweep` 已经是一个 set-based SQL 操作——通过策略表进行间接操作是可行的。`data_class` 的初始默认值（将现有数据分类为 `UserContent`）是一场一次性的回填迁移。

### 方向 4：Block 版本协商与兼容合约（P1）

**为什么需要**：`Block` 枚举是 Aero 中最重要的序列化类型——它跨越客户端、网络、存储和 AI 上下文。尽管如此，它**没有**版本字段、`deny_unknown_fields`（意味着未来字段会被静默丢弃而非优雅拒绝）、以及用于客户端声明的 `block_caps` 握手。服务器可以发布一种客户端不理解的 `Block` 变体——Web 端呈现 `[${b.type}]`，移动端客户端崩溃。随着 AI 代理开始生成新的块类型（`tool_call` 已经出现），这个问题会变得更糟。

**核心挑战**：
- `Block` 需要 `#[serde(deny_unknown_fields)]` 来实现已知未知的严格拒绝——但这会在服务器和客户端版本不匹配时产生兼容性断裂。解决方案是客户端版本化：WS `Welcome` 帧必须传达服务器支持哪些 `block_caps`（枚举已识别的块变体），且客户端在发送前验证自己的理解。
- 旧客户端遇到新块类型时的降级策略必须结构化：`default` fallback 呈现 `[tool_call]` 是第一步，但交互式块（`button`/`select`）在降级时丢失功能。一个更优的解决方案是服务器端渲染回退：当客户端不声明 `caps` 包含 `interactive_blocks` 时，服务器将 `button` 块替换为文本回退或链接。
- 回填：为序列化的 `Block` 实例添加版本身标识（例如，在枚举外层包裹 `{ "v": 1, "blocks": [...] }`）为未来的迁移路径创建契约。

**预期变更**：
- `ServerFrame::Welcome` 添加 `block_caps: Vec<String>` 字段（例如 `["text", "code", "mention", "file", "voice", "card", "tool_call", "thought", "button", "select", "interactive"]`）。
- 在 `block.rs` 中添加 `BlockVersion` 包装类型。
- 在 Web 渲染器中：根据当前 `block_caps` 验证 `b.type`；更新 `appendBlock` 以发出结构化降级警告（可被路由至遥测）。
- 将 `deny_unknown_fields` 应用于 `Block`（或上层包装器）。
- `Card` payload 模式注册表：`schema` 字段已存在但未经验证——添加模式验证端点和已知模式的注册表。

**对现有系统的影响**：中。`Welcome` 帧变更会影响所有 WebSocket 消费者。`deny_unknown_fields` 是运行时行为变更——应在服务器发布新块类型**之前**部署，以免拒绝有效负载。

### 方向 5：媒体管道统一摄取抽象（P2）

**为什么需要**：当前，三种摄取路径（RTMP、WHIP、SRT）各自拥有独立的生命周期管理、指标和错误报告。`go_live_hook` 以一种独特的方式桥接它们（通过 `LiveStreamConfig::go_live` 无总线钩子）。一个统一的 `LiveIngest` trait 已存在于 `aero-live-core`，但三种实现的保真度不均——WHIP 有注册表，SRT 有会话计数器，RTMP 有 tcp 监听器。这种自相似性在故障排除时增加了认知负荷，并使得每个新摄取路径需要重复工作。

**核心挑战**：
- 抽象必须接纳三种协议的截然不同的传输特性：RTMP 是 TCP 长连接，WHIP 是 HTTPS + RTP over UDP，SRT 是自定义 UDP 有损恢复。然而，在所有三种之上，存在共同的横切关注点：会话启动/关闭、HLS 段生产者绑定、指标注册、以及错误分类。
- 理想情况下，`LiveIngest` 应提供默认实现：当与会话关联时，指标（计数器和仪表）自动注册/注销——这已在 SRT 的 `SessionCounter` 中实现，但未在 trait 本身中形式化。

**对现有系统的影响**：低至中。主要是重构——将中央 crate 中的逻辑内聚（`aero-live-core`），并从三个实现中提取每个协议特定的适配器。

---

## 3. 接口设计建议

### 关键接口原则

1. **事件 → 操作 → 审计链**：每个 bot/AI 操作应跟随可审计的踪迹。所有 bot 调用应路由通过一个中央 `BotRuntime` trait，该 trait 在调用前执行配额/撤销检查，并在调用后记录。签名应为：
   ```
   trait BotRuntime: Send + Sync {
       async fn dispatch(&self, bot_id, event, ctx) -> Result<DispatchOutcome, BotError>;
       async fn revoke_token(&self, token_id) -> Result<(), AuthError>;
   }
   ```

2. **媒体指标应为第一类接口**：每个 media-plane crate 应暴露一个 `pub mod metrics`（遵循 SRT 的模式），由中央 `MetricsRegistry` 注册。名称应遵循层级命名空间以避免冲突：
   ```
   aero_live_whip_rtp_packets_received_total  // current (good)
   aero_live_srt_packets_received_total        // current (good)
   aero_live_sfu_forwarded_packets_total       // proposed (follows pattern)
   aero_live_hls_segment_duration_seconds      // proposed (follows pattern)
   ```
   跨 crate 共享的名称放在 `aero-common` 中；crate 局部的名称放在各 crate 中。*不要*提取所有名称到 `aero-common`——这会使依赖图膨胀。反之，保持 crate 局部名称并添加单元测试，以防被意外重命名（SRT 已经这样做了）。

3. **数据策略声明式，非命令式**：数据生命周期规则应在策略引擎中表示为数据，而非在 sweep 函数中作为 SQL 子句。策略记录应为：
   ```
   (data_class, action) → { ttl_days, encryption_required, mask_pattern, exempt_roles }
   ```
   这使得法务保全可以作为一条策略记录插入（`(UserContent, DELETE) → { exempt: true }`）并自动集成，而无需修改清扫 SQL。

4. **Block 版本合约明确**：`ServerFrame::Welcome` 必须包含 `block_caps`。`Block` 序列化必须包含版本身标识：
   ```json
   { "v": 1, "blocks": [...] }
   ```
   这允许服务器在必要时用 `"v": 2` 发出块，旧客户端在看到不支持的版本时显式降级（通过服务器端渲染），而非静默破坏。

### 新的抽象层

- **`BotRuntime`**：将 bot 注册、调度、配额、撤销和审计日志记录整合到一个 crate 中。这是 P0 扩展所必需的。
- **`DataPolicyEngine`**：用于评估 `(data_class, action, context) → policy` 的声明式规则引擎。对于 P1 数据生命周期治理是必需的。
- **`BlockRegistry`**：已知 `Block` 类型、它们的序列化形式以及客户端能力要求的注册表。这是 P1 块版本化的核心。

### 向后兼容性

- 所有新字段应为 `Option` 类型或具有通过 `#[serde(default)]` 提供的默认值。
- WS `Welcome` 帧应添加 `block_caps` 作为可选字段以避免破坏现有客户端。当缺失时，服务器假设旧客户端并发送版本 1 块。
- 策略引擎的默认规则集应镜像现有行为（`retention_days` → `UserContent` TTL；`legal_holds` → 覆盖）。
- `deny_unknown_fields` 应在**已与 cap 握手的客户端**上启用，而非全局启用。包装器类型可以实现这一点。

---

## 4. 技术选型

### 不需要新框架

所提出的扩展方向**不需要**新的主要运行时依赖。当前技术栈（tokio + Axum + sqlx + fred + async-nats + str0m）足以支持所有方向的演进。具体来说：

| 方向 | 关键依赖 | 说明 |
|---|---|---|
| Bot 运行时 | 当前技术栈 | 使用 `ai_jobs` 模式，添加 `audit_bot_actions` 表 |
| 媒体观测 | `metrics` crate（已在使用）+ `tracing` | 无需新框架；热路径聚合需要小心 |
| 数据分类 | 当前技术栈 + sqlx | 策略引擎可以是纯 Rust + 一个 DB 表 |
| Block 版本化 | serde（已在使用） | 当前 `serde_json::Value` 配合 `deny_unknown_fields` |
| 统一摄取 | 当前 trait（`LiveIngest`） | 重构而非引入 |

### 自建 vs. 采购评估

| 边界 | 决策 | 理由 |
|---|---|---|
| **Bot 配额** | 自建 | 高度耦合于 AI 预算系统（`CostBudget`，`AiWorker`）——通用配额库无法理解 AI 调用权重的语义。自建 ≤ 500 行。 |
| **数据策略引擎** | 自建 | 规则集很小（< 20 条规则）。通用策略引擎（OPA，Casbin）会增加 5 倍以上的二进制体积和部署复杂性。一个匹配 `(data_class, action)` 的枚举 + 决策表（BTreeMap）就足够了。 |
| **媒体仪表盘** | Grafana（既有） | 使用 Grafana 的面板库。无需购买。 |
| **块模式验证** | 当前技术栈 | `Card` 模式可以是一个 `HashMap<String, JsonSchema>`（通过 `schemars` crate），在 POST 时验证。无需服务。 |
| **链路追踪传播** | 当前技术栈 | `tracing` 的 `Span` 通过 `tracing-opentelemetry` 配合 OTLP exporter。已在配置中（`AERO_TRACE_SAMPLE_RATE`）。 |

### 应监控的评估标准

1. **数据依赖**：是否引入了该 crate 不拥有的新 DB 表？（坏：`aero-live-webrtc` 直接写入 `aero-storage` 的表。好：通过 `aero-bus` 事件传递。）——当前遵守得不错。
2. **启动时间**：新 crate 是否增加 >50ms 的编译时间？（对于开发工作流是可以接受的。）关注 `schemars` 派生——`Card` 模式注册表应可选编译或按需加载。
3. **配置表面**：新扩展是否增加了 >3 个环境变量？（目前 `AERO__*` 命名空间通过预分片管理良好——保持在 20 个以内新配置键。）

---

## 5. 实施路线图

### 优先级框架

| 优先级 | 判定标准 | 方向 |
|---|---|---|
| **P0** | 安全关键（数据丢失/违规）或基础设施封锁后续迭代 | 方向 1（Bot 运行时），方向 2（媒体观测——SFU/HLS 指标差距） |
| **P1** | 产品质量差距；上线所需 | 方向 3（数据分类），方向 4（Block 版本化） |
| **P2** | 生产力/可维护性提升；FOMO 可接受 | 方向 5（统一摄取），方向 2b（SFU HA） |

### 第一阶段：基础治理（4-6 周）

**重点**：Bot 运行时 + 媒体观测最低可用产品

- **第 1 周**：审查当前 bot 入口（`background.rs`，7 个 bot 模块）。提取共同的调度/配额/审计模式到 `BotRuntime` trait。为 `agent_bot` 添加配额检查（同步 AI 路径是最紧迫的）。
- **第 2 周**：向 `aero-live-webrtc` 和 `aero-live-hls` 添加本地 `metrics.rs` 模块。从 `aero-common` 的度量名称集中提取共享度量名称。
- **第 3 周**：bot 取消令牌吊销（集成到现有的 PAT 撤销基础设施）。
- **第 4 周**：审计日志记录——添加 `audit_bot_actions` 表 + 在 `BotRuntime::dispatch` 中进行写入。
- **第 5-6 周**：整合、仪表盘创建、负载测试以验证配额强制执行。

**风险**：
- agent_bot 的同步 AI 路径与异步 `ai_jobs` 预算系统存在阻抗不匹配。缓解措施：为 agent_bot 添加一个轻量级的进程内令牌桶（每个工作区/每个 bot），独立于全局 AI 预算。actor_bot 被认为风险太低而无需全局预算——但这种认知是不正确的。

### 第二阶段：数据生命周期 + 块版本化（6-8 周）

**重点**：数据分类迁移 + 版本协商

- **第 1-2 周**：设计 `DataClass` 枚举 + 迁移添加可为空的列。回填现有行为（现有数据 → `UserContent`；审计表 → `AuditLog`）。
- **第 3 周**：实现 `DataPolicyEngine`（匹配 `(data_class, action)` 的枚举决策表 + 可选的 DB 备份）。策略驱动点：`retention_sweep`、`encryption_at_rest` 包装器。
- **第 4 周**：法务保全集成：清理 `legal_holds` 当前扫描 `rooms` 的 `NOT EXISTS` 子查询。用策略驱动的方法替换，以消除全表扫描。
- **第 5 周**：区块版本化——将 `BlockVersion` 包装器添加到序列化中。更新 `ServerFrame::Welcome`，添加 `block_caps`（做向后兼容选项）。
- **第 6 周**：将 `deny_unknown_fields` 应用于具有 cap 感知序列化的区块。
- **第 7-8 周**：Web 客户端更新以处理 `block_caps` 握手 + 结构化降级。`Card` 模式注册表（可选验证端点）。

**风险**：
- Data backfill for `data_class` on a large production database (`UPDATE ... SET data_class = 'UserContent' WHERE data_class IS NULL`) will lock the table. Mitigation: batch in chunks of 10,000 rows with `NOWAIT`; the column is nullable so consumers treat NULL as `UserContent` during the migration window.
- `deny_unknown_fields` is a runtime behavior change. If a new client sends a field an old server doesn't recognize (unlikely during a controlled rollout) the connection drops. Mitigation: rollout `deny_unknown_fields` on the **server side** only after all clients are updated, or gate it behind the `block_caps` handshake.

### 第三阶段：平台演进（随时，P2）

**重点**：统一摄取抽象，SFU HA 评估

- 将 `LiveIngest` 指标和生命周期管理提取到 `aero-live-core`。三个摄取路径迁移到共享模式。
- 评估 SFU 外部化的业务需求（直播 HA 是必要条件吗？还是通话优雅降级可以接受？）。
- 如果追求 SFU HA：将路由表外部化到 Redis（遵循 `stream_route` TTL 模式），并添加客户端重连逻辑。

**风险**：低——这些都是可选的、面向生产力的重构。如果 P0 和 P1 交付了主要价值，推迟完全没有问题。

### 总体风险与缓解措施

| 风险 | 可能性 | 影响 | 缓解 |
|---|---|---|---|
| Bot 配额与 AI 预算系统不兼容 | 中 | 高 | 在第 1 阶段添加进程内令牌桶以避免耦合；后期统一 |
| data_class 回填在 PG 上锁表 | 中 | 中 | 批量迁移 + 可空回退读取 |
| Block 版本化破坏现有客户端 | 低 | 高 | WS `block_caps` 作为可选字段逐步推出；先在新客户端上进行内部测试 |
| 媒体指标热路径开销 | 低 | 中 | 批量计数器更新；使用 `metrics` crate 的 `Counter` 而不是原始原子操作 |
| 第二阶段的工程注意力分散 | 中 | 中 | 严格阶段划分——在第 1 阶段稳定（4-6 周）之前不开始第 2 阶段 |

### 关于交叉引用表注释

我确认在保存前应纠正文档中较小的引用偏差。三个实质性修正：

1. **SRT/WHIP 指标存在**：文档关于媒体指标“零”的论断应该限定在 SFU 和 HLS 上——SRT 和 WHIP 各自拥有结构良好的指标模块（4 个和 3 个计数器/仪表）。主题句应为“SFU 和 HLS 零指标”，而非“所有媒体 crate 零指标”。

2. **`searchable_text` 是 Rust 侧方法**：`Block::searchable_text()` 位于 `block.rs:142`——这是一个由 `Message::searchable_text()` 调用的 Rust 方法，反过来由 `search_query.rs` 的存储库层使用。不存在名为 `searchable_text` 的 JS 函数。文档关于函数名/位置/语言的说法应修正，或在脚注中注明。

3. **`Poll` 不是区块变体**：区块枚举中应删除对 `Poll` 的引用——它是一个独立的 REST API + `PollEvent` RoomEvent，其有效负载形状与 `Block` 不同。

这三处修正并不会削弱核心论点——四道交叉断层在代码验证后依然稳固——但它们使文档在 grep 锚点方面更加精确。
