好，我已经完整阅读了 `2026-07-11-strategic-expansion-analysis-mobile-bot-event-platforms.md` 以及整个工程的 AGENTS.md 架构规范。以下是架构视角的深度分析。

---

# 架构分析：Aero IM 战略扩展方向的技术评估与实施建议

## 1. 架构评估

### 1.1 当前架构的优势

**事件 DAG 设计是系统最坚固的架构决策。** 从 IM 消息到直播弹幕到通话信令，全部复用同一模式：业务层产事件 → NATS subject 投递 → 进程内 Hub 扇出 → WebSocket。这个模式和几个关键实现细节让它比大多数自建实时系统更健壮：

| 优势 | 具体体现 |
|---|---|
| **跨实例一致性** | NATS JetStream durable consumer (`aero-server`) 提供 at-least-once 语义 + 按序投递，水平扩多实例时自动负载均衡，不会出现"同一个房间的事件在不同实例间乱序" |
| **进程内扇出隔离** | `Hub::fan_out_raw` 用 bounded mpsc，从 bus listener 到 WS 连接有显式背压，不会让 NATS consumer 无限积压在内存 |
| **per-subject 单调 seq** | `bus/seq.rs` 在每个 subject 命名空间维护独立 seq，即使 NATS 重投同一事件，客户端也能通过 seq 去重/排序 — 这是很多实时系统忽略的设计 |
| **crate 分层禁止成环** | 从 `aero-common`（叶子）→ `aero-bus`/`aero-storage`/`aero-auth` → `aero-im-core` → `aero-server` 的依赖方向清晰，无循环依赖 |
| **功能隔离粒度为 crate** | 15 个 crate，每个 crate 是一个 feature 单位。这种粒度比模块目录更有利于独立编译、独立测试、未来独立部署 |

**但真正让我印象深刻的是 `bot_dispatch.rs` + `BotEventSubscription` 的设计。** 虽然文档称"内置 bot 硬编码"是缺口，但底层的*投递管道*已经模块化得很好：事件 → NATS → 各 bot 的 durable consumer → `BotDelivery`（含 backoff + status 追踪 + 失败滚存）。Direction 2 的 bot 平台不是从零建管道，而是在现有管道上**加注册层和协议层**。

### 1.2 架构局限性与技术债

有六个结构性问题值得讨论：

**（一）单集群 NATS 是单点故障边界——不仅是跨区域问题。** 当前 `run_bus_listener` 是一个 durable consumer 连接到一个 NATS 集群。这个集群挂了，整个系统的实时性归零（消息仍能通过 REST API 持久化，但 WebSocket 扇出、直播弹幕、通话信令全部中断）。Direction 3 正确地识别了这个缺口，但要注意：NATS SuperCluster 的拓扑不是代码改动，是基础设施基建，需要提前规划网络延迟预算。

**（二）WebSocket 连接的进程绑定问题。** 当前架构中，客户端 WS 连接到某实例后，`Hub::fan_out_raw` 只扇出到本实例的 WS 连接。如果一个用户连接到实例 A，而房间事件由实例 B 的 `run_bus_listener` 接收并解包，那么事件要经过：B 的 `run_bus_listener` → B 的 `Hub` → B 的 WS → 网络 → A 的 WS？不，那是错误的。实际上 NATS 是广播给所有实例的 durable consumer，每个实例的 `run_bus_listener` 都独立消费事件并扇出给本地的 WS 连接。这意味着 `im.room.*` 事件的副本会发送给**每个实例**，每个实例独立 decode 并检查房间成员，然后只扇出给本实例的连接——这是一种"读放大"。

```
[实例A] run_bus_listener → decode → check members(房内在线的是A的用户) → fan_out → A的WS连接A
[实例B] run_bus_listener → decode → check members(房内在线的是A的用户，B无在线连接) → drop
```

这不是 bug，而是设计取舍——每个实例都看到所有事件，但用 `explicit_recipients`（AGENTS.md 描述的"成员展开"）来决定扇出给谁。对于 IM 场景（房间数远大于连接数），这种读放大的代价是可控的。但如果房间内有一万个连接分散在100个实例，每个实例仍然要 decode 并检查一万次成员关系。这个模式在百实例级别需要优化。

**（三）前端架构债比文档描述得更严重。** 文档列出 `context.js` ~208 行是"全局状态杂货铺"，并统计了约 35-40 个功能的 UI 覆盖率。但我更关注的是**缺少前端测试、缺少路由机制、缺少模块化打包**这三个基础设施层面的缺失：

- `eslint.config.js` 已到位，但 `eslint` 只能检查语法/风格，不能验证一个 WS 事件是否被正确的 UI 模块处理
- 没有 bundle step 意味着每模块靠 `<script src="...">` 标签引入，依赖 HTML 构建 DAG，无法使用 `import` 语句进行真正的模块间边界控制
- `polls.js` ~169 行确实是好的独立模块范式，但它的独立性来自"不引用 context.js"的手动克制，不是由架构强制

**（四）AiWorker 预算系统是精密的架构，但也是操作复杂性的来源。** per-ws 60 / global 300 / per-kind weight / FAILS-CLOSED on spend / defer 不消耗 retry / `DEAD-LETTER MAX_ATTEMPTS=5`。这套系统的设计文档在 AGENTS.md 里只有两段，但实际行为涉及至少 6 个状态的交互。如果要暴露给第三方 bot（Direction 2），需要想清楚：第三方 bot 是否受同一套预算约束？还是用独立的 per-app 配额池？

**（五）媒体 seam 的"已建+已测+未接线"状态是隐形的架构风险。** `call_bridge_supervisor::ensure_egress` 有单元测试，`SfuMediaSession::run` 有结构但只被 `#[cfg(test)]` 调用。AGENTS.md 明确说"别当未做重造，也别写'完成'"——但这个 seam 的状态意味着跨节点通话桥接在真实多实例部署中完全静默。如果未来某天紧急需要通话桥接，团队需要排查为什么 egress 不工作，可能会在"代码逻辑错误"和"代码根本没被调用"之间浪费 debug 时间。建议：在 boot 阶段加一个 `tracing::warn!("SFU media session not wired — cross-node bridging is inactive")` 日志。

**（六）`commands.rs` 的架构耦合比文件本身显示得更深。** 当前的 `match name { "me" | "shrug" | "giphy" | "remind" | other => Error }` 不是单纯的模式匹配——它嵌入在 `send_message` 的 WS 处理流程中，在消息持久化之前拦截。如果要让第三方 bot 注册命令，按照当前架构，要么修改 `commands.rs` 加入一个 `HashMap<String, Plugin>` 注册表（文档建议），要么让 `commands.rs` 在未匹配时发射一个 `BotEvent::UnknownCommand` 经过 bot delivery 管道。后者的可扩展性更好，但代价是延迟增加一次 HTTP 回调。

### 1.3 关键设计决策是否合理

| 决策 | 当前状态 | 评估 |
|---|---|---|
| **事件总线选 NATS 而非 Kafka** | ✅ 当前 | **正确。** IM 场景需要的是小消息、低延迟、按 subject 扇出，NATS JetStream 在这方面比 Kafka 更轻量（无 partition rebalance 延迟、无磁盘写放大）。Kafka 的优势在大吞吐量日志场景，而非实时 IM。 |
| **跨实例状态走 Redis 而非内存广播** | ✅ 当前 | **正确。** Presence 和 roster 需要跨实例可见性，但可以接受最终一致性。Redis sorted-set 的 `zadd`+`zremrangebyscore` 是精确匹配这一需求的原语。NATS KV Store 也行但少了 sorted-set 的过期驱逐能力。 |
| **Web SPA 零依赖 ES2020** | ❌ 应重评估 | 对 MVP 阶段合理（零依赖 = 零 build 工具链 = 快速迭代）。但对 95+ 后端功能模块只有 ~40 个 UI 覆盖率的现状，零依赖架构已经成为前进的**制动器**——不是因为不能加 UI，而是因为没有模块边界、没有测试、没有 bundle 拆分使并行开发多个 UI 模块变得困难。建议在方向四中把"引入最低程度的模块化基础设施"（如 esbuild 做 bundle + TypeScript 类型检查）列为前置步骤。 |
| **SFU 进程内而非独立媒体服务** | ✅ 当前 | **正确但有限度。** 对于单机部署或小集群，str0m 进程内 SFU 消除了额外的 RTP relay 延迟。但当集群超过 10 个节点时，每个节点上独立的 SFU 实例会对 NATS/Redis 产生 O(N²) 的连接数。届时应考虑独立 SFU 集群（类似 LiveKit 的架构），但那是规模问题，非当前阶段。 |
| **音频/视频通话走同一事件通道** | ✅ 当前 | **正确。** 信令事件（`call_*` WS 帧）和媒体面（RTP/DTLS）分离，信令走 NATS 事件通道、媒体面走 str0m SFU + call-bridge UDP。这是业界标准模式（每 WebRTC 应用都是这样），Aero IM 的实现是干净的。 |

---

## 2. 扩展方向——从架构视角审视 5 个方向

### 2.1 方向二：Bot/App 平台（P0，架构优先级最高）

**为什么要从这个方向开始：** 不是因为产品价值最高（方向五的产品价值可能更高），而是因为它对现有架构的**增量最小、复用最大**。它几乎完全坐落在已建的 `BotEventSubscription` + `BotDelivery` + `webhook::build_delivery` 管道上。需要的架构变更主要是**注册层的扩展**和**命令路由的从 match 到注册表**的演进。

**核心技术挑战：**

1. **命令路由降级策略**：当 `commands.rs` 从 `match` 切换到 `Registry`，要考虑"注册的命令比内置命令优先级低还是高？"如果第三方 bot 注册了 `/help`，它应该覆盖内置的 `/help` 吗？建议：内置命令优先，注册命令只能填未覆盖的命名空间。注册 `giphy` 应该被拒绝（已存在），注册 `translate` 应该被允许。

2. **权限模型必须在第一天设计（即使 OAuth 延后）**：不使用 OAuth 而直接用 workspace token 授权时，权限粒度应该是什么？"安装时一次性声明所需 scope"还是"每次都问用户"？建议参照 Slack 的 Bot Token scope 模型：安装时声明 `commands:write` `messages:read` `channels:history` 等 scope，执行时在 `assert_room_access` 层加一个 `app_scope_guard` 检查。

3. **Interactive component 回调的安全模型**：当前 `interactions.rs` 写 `block_interactions` 表 + 广播 `RoomEvent::Interaction`。如果要回调到 bot 的 `interactive_url`，需要验证回调请求确实来自 Aero IM 服务器。建议使用已有的 `webhook::hash_token` 方案（HMAC-SHA256 signature header），不引入新加密机制。

**预期的架构变更：**

```
方向二架构变更（最小变更集）
┌─────────────────────────────────────────────────────────────┐
│ 现有的：                                                    │
│  WS:send_message → commands.rs(match) → inline handle      │
│  WS:block_interaction → interactions.rs → DB write + event │
│  NATS:im.room.* → bot_dispatch → BotDelivery → webhook     │
├─────────────────────────────────────────────────────────────┤
│ 扩展为：                                                    │
│  1. commands.rs 加 CommandRegistry trait + load_registered()│
│  2. WS:send_message → commands.rs(registry match)          │
│     ├── 内置命中 → inline handle                            │
│     └── 注册命中 → BotEvent::Command → bot_dispatch → HTTP │
│  3. interactions.rs 加回调：                                │
│     DB write + event + if app.interactive_url → build_delivery
│  4. POST /api/apps → AppRepo + install_flow                 │
└─────────────────────────────────────────────────────────────┘
```

这个变更集**不**需要新 crate（初期可放在 `aero-server/src/apps.rs`），当 grow 到需要独立的 Bot 注册表+AppManifest 解析+权限校验矩阵时再提取为 `aero-app-platform`。这和 AGENTS.md §4 的"feature-first"规则一致——先内联，到阈值再拆 crate。

### 2.2 方向五：结构化虚拟活动（P0-P1，产品价值最高，技术风险最高）

**架构层面的深层问题。** 文档将方向五描述为"将已存在的能力组合"，但架构上看，真正的挑战不是"组合"而是**跨子系统的状态协调**。

当前系统各能力之间的依赖关系是：

```
scheduled_stream (调度) → WHIP/RTMP (推流) → HLS (分发) → stream_chat (互动)
                                                                     ↓
                                                              poll/预测/礼物
                             
call: 1:1/群通话 (独立子系统，经由 CallOrchestrator 管理状态)

VOD/recap/chapters (录制后处理，无实时依赖)
```

一个"虚拟活动"的端到端流程需要串起来的是：

```
Event (创建+编排) 
  ├→ 注册/票务 (新)
  ├→ Session 1 (scheduled_stream + WHIP推流 + HLS播出 + 弹幕互动)
  │   └→ Q&A面板 (新，但复用互动基础)
  ├→ Session 2 (不同主讲人，自动切换HLS playlist)
  ├→ 录制合成 (VodAssembly, 新)
  └→ AI摘要+章节 (现有 call_recap + vod_chapters)
```

**核心架构决策：事件编排层放在哪里？**

| 选项 | 优点 | 缺点 | 推荐度 |
|---|---|---|---|
| A: 新 crate `aero-event-orchestrator`，独立状态机 + NATS 订阅 | 职责单一，不耦合现有 crate，独立 scale | 对现有系统的 API 调用增加跨 crate 间接性 | ⭐⭐⭐⭐ P2 阶段 |
| B: 扩展 `aero-live-core`，增加 Event 实体 + Session 子表 | 复用直播流的状态管理，ScheduledStream 和 EventSession 一个 repo | 事件和直播流的耦合加深，未来解耦困难 | ⭐⭐ Phase 1 OK |
| C: 扩展 `aero-im-core`，ImService 加 event_* 方法 | 最快，复用 ImService 的 participant/room 上下文 | ImService 已经是 100+ 方法的上帝类，再加 event 让它更胖 | ⭐ Phase 1 over **不推荐** |

**我的建议：分阶段。**

- **Phase 1 (3-4 周)**：在 `aero-live-core` 中扩展 `ScheduledStream` → 增加 `event_type` 字段（`simple_stream` / `multi_session` / `webinar`）+ `EventSession` 子表 + `POST /api/events` CRUD。这个阶段没有"活动编排"，只是扩展了数据模型。

- **Phase 2 (4-6 周)**：`aero-live-core` 的 `stream_create` 逻辑增加 "event session 切换" 能力：当 WHIP 推流结束时如果 event 有下一个 session，自动创建新的 stream + 更新 HLS playlist。这是真正的编排开始。

- **Phase 3 (8-10 周)**：当编排逻辑变得足够复杂（等待室、Q&A 流程、举手队列、权限区隔），提取为 `aero-event-orchestrator` crate。此时有充分的测试边界来指导 crate 的接口设计。

**ffmpeg 依赖的架构考量：** 文档记录 ffmpeg 是"硬依赖"，我同意。但建议起始阶段不依赖 ffmpeg——多 Session 的录制合成可以先做"虚拟 HLS playlist"：所有 session 的 `.ts` 片段保留，生成一个按时间顺序排列所有片段的 `.m3u8`。这对播放器来说就是完整回放，无需 re-encode。等到需要导出为 `.mp4` 下载时才引入 ffmpeg。这样方向五的 Phase 1 和 Phase 2 保持纯 Rust 栈。

### 2.3 方向一：Web Push + Tauri 桌面壳（P1，低成本高收益）

**架构层面的核心问题不是"加模块"，而是"客户端状态生命周期管理"。**

Web Push Protocol (RFC 8030) 的实现在服务端很简单——`WebPushGateway` 实现 `PushGateway` trait，VAPID 密钥生成 + 加密 payload + POST 到浏览器 Push Service。问题在客户端：

1. **Service Worker 生命周期**：Web SPA 是纯前端无构建工具。Service Worker 需要独立打包、注册、更新策略。当前 `<script>` 标记架构意味着 SW 文件必须在根目录、且不能有 import 语句。这是可行的（Google Workbox 不需要），但需要 SW 代码和 SPA 代码完全分离。

2. **通知点击处理**：用户点击推送通知时 SW 需要知道 deep link 到哪个房间。这意味着推送 payload 中需要 `room_id` 和 `message_id`，SW 使用 `clients.openWindow('/rooms/' + room_id)`。但当前 `push_bot.rs` 的 payload 构造在服务端，需要扩充。

3. **Tauri 壳的回调机制**：Tauri 的 Rust 后端可以通过 `tauri::api::notification` 发原生通知，但需要从 Web SPA 的 WS 连接跨过 WebView 边界到 Tauri Rust 端。建议使用 Tauri 的 event system（`app.emit("notification", payload)`），不引入额外 IPC 复杂度。

**这三个挑战中，前两个是要解决的，第三个已由 Tauri 框架解决。** 总体来看 Web Push + Tauri 的架构成本大约 4-6 周（文档估 8-12 周偏保守），其中 Tauri 壳本身 2 周（现有 SPA 改 `<meta>` + `tauri.conf.json` 配置），其余是 SW + 原生通知模式适配。

### 2.4 方向四：前端完成度（P1，必须与方向二/五并行）

**架构核心不是"加 UI"，而是"控制混乱的熵增"。**

文档列出了约 28 个缺失 UI 的后端功能，并建议每模块 2-5 天。但如果**没有前端架构的支架**，加 28 个 `web/*.js` 文件会让问题从"40 个功能分布在 ~5900 行中的状态杂货铺"变成"68 个功能分布在 ~10000 行中的状态杂货铺"。先加架构，再加功能。

**建议的前端架构增量步骤：**

1. **引入 esbuild（3-5 天）**：不是因为需要 TSX/JSX，而是为了：
   - `import` / `export` 语句（解决模块边界问题）
   - `--bundle` 输出单个文件（减少 HTTP 请求，当前 ~30 个 `<script>` 标签）
   - `--watch` 开发模式 + source map
   - 零配置，零依赖到 `package.json`（esbuild 是 Go 二进制，不依赖 Node 生态）
   - `.json` imports 在 esbuild 中天然支持（当前没用到，但未来多语言需要）

2. **状态管理分层（2-3 周）**：
   - `state/messages.js`：消息增删改事件 → 更新本地消息缓存
   - `state/rooms.js`：房间列表/未读数/在线成员
   - `state/streams.js`：直播流状态/观看数
   - `state/ui.js`：当前路由/选中的房间/打开的 drawer
   - `context.js` 收缩为这些 store 的初始化编排点 + 向后兼容的 getter 函数

3. **功能覆盖检查（持续）**：扩展 `scripts/web-check.sh`：扫描 `server/src/routes/routes.rs` 的 `.merge()` 链，提取模块名，检查 `web/` 下是否有对应的 `.js` 文件。任何新路由模块如果没有对应的前端文件，输出 WARN 但不 fail。这建立了一个"后端功能↔前端覆盖"的透明映射。

**结论：方向四做不做？要做。但先花 3 周重构前端基础设施，再用 4-6 周并行产出 6-8 个高优先级 UI 模块（画布/任务/审批/频道管理/创作者中心），而不是 28 个并行。**

### 2.5 方向三：多区域/边缘部署（P2，按业务节奏后置）

**架构上最重、收益在基础设施层。**

这个方向的真正架构挑战不是 NATS SuperCluster 或 Redis 跨区域——这些是已知方案。真正难的是**应用层的区域感知**。

当前架构中，`ImService::publish_room_event` 完全不知道"哪个区域应该处理这个房间"。NATS subject `im.room.{id}` 是全局的。要让事件按区域路由，需要：

1. `rooms.region VARCHAR`（文档已提）
2. `publish_room_event` 根据 `room.region` 选择 NATS 连接（区域级 vs 全局 subject）
3. 跨区域 room 的事件需要复制（通过 NATS gateway subject）
4. 写操作需要路由到 region 的主 PG（避免复制延迟）

**这是应用层全链路改造，不是基础设施层配置。** 需要改的 crate 包括：

| crate | 变更 |
|---|---|
| `aero-common` | Config 层多区域连接串、`Region` 类型 |
| `aero-bus` | `EventBus` trait 扩展 region-aware publish + cross-region subscription |
| `aero-storage` | PgPool per region + 写路由逻辑 |
| `aero-im-core` | `publish_room_event` 按 region 选择 subject |
| `aero-server` | 启动时初始化多区域连接 + region-aware healthcheck |

我的建议是：方向三不要在 DAU < 100k 跨区域分布前作为独立方向投入。但**架构设计上要预留**——`aero-bus` 的 `EventBus` trait 加一个 `region: Option<Region>` 参数；`rooms.region` 迁移可以在 Phase 1 只加字段不做路由。这些预留成本几乎是零。

---

## 3. 接口设计建议

### 3.1 Bot/App Plugin 接口设计原则

方向二需要定义一个 `AppHandler` trait，它是整个 bot 平台的核心抽象。建议的设计原则：

1. **一个 trait 覆盖三种交互**：command 处理、interactive component 回调、event subscription。bot 不需要三种都实现，但通过同一个 registration point 注册，简化路由。

2. **请求验证通过签名头**：沿用 `webhook::hash_token` 的 HMAC-SHA256，不引入 OAuth 的额外加密复杂度（OAuth 仅在授权流阶段使用）。

3. **响应模型严格定义**：bot 回调的 HTTP 响应 body 决定附加行为——`{"blocks": [...]}` 在会话中追加消息，`{"message": "..."}` 仅回复文本，`{"ack": true}` 无附加行为。不要在响应体中返回 HTML、markdown 或其他富文本格式，保持在 `Block` 模型的边界内。

### 3.2 跨 crate 的事件契约规范

方向五中，Event Orchestrator 需要和多个 crate 交互。如果不规范接口，容易出现方向五的典型问题：Event Orchestrator 直接依赖 `aero-live-core` 和 `aero-im-call` 的内部类型。

**建议：为跨 crate 边界的数据流定义契约模型，放在 `aero-common` 中。**

```
aero-common
  └── model/
      ├── event.rs        ← Event 实体（workspace_id, title, start_at, ...）
      ├── event_session.rs ← 子 session（speaker, duration, stream_id）
      └── event_stage.rs  ← 阶段转换（Draft→Published→Started→Ended）
```

`aero-event-orchestrator` 只依赖 `aero-common`（模型）+ `aero-bus`（发布事件）+ `aero-storage`（读写表）。它调用 `aero-live-core` 创建 stream 是通过 `ImService` 的公共方法，不是直接调用 repo。

### 3.3 前端 State Layer 接口约定

方向四的状态管理层需要约定接口：

```
// 每个 state 模块暴露：
export function subscribe(eventType: string, handler: (data) => void): UnsubscribeFn;
export function getState(): Readonly<State>;
export function dispatch(eventType: string, payload: unknown): void; // 仅由 ws.js 调用
```

```
// ws.js 不直接调用 context.js，而是：
import { dispatch } from './state/messages.js';
import { dispatch as roomDispatch } from './state/rooms.js';

ws.on('msg:message', data => dispatch('MESSAGE_RECEIVED', data));
ws.on('msg:room_updated', data => roomDispatch('UPDATED', data));
```

这个约定的核心价值：**UI 组件不直接监听 WS 事件**，它们通过 state layer 订阅变更。这使测试变得更简单（测 state layer 时 mock WS，测 UI 组件时 mock state）。

### 3.4 Event Orchestrator 的对外接口

方向五的 Event Orchestrator 应该暴露 REST API 给 web UI、发射 NATS 事件给 bus listener、但**不直接访问 WS 连接**。它的接口契约：

```
// REST: 给 web UI 用
POST   /api/workspaces/:wid/events              → create_event
PATCH  /api/workspaces/:wid/events/:id          → update_event
POST   /api/workspaces/:wid/events/:id/publish  → publish_event (Draft→Published)
POST   /api/workspaces/:wid/events/:id/start    → start event
POST   /api/workspaces/:wid/events/:id/end      → end event
GET    /api/workspaces/:wid/events              → list (filterable by status)

// NATS: 内部状态转换通知
live.event.{id} → Status → run_event_bus_listener → hub.fan_out

// 不提供：
// 直接访问 WS 连接
// 同步直播流控制（通过现有的 stream_create/stop 接口）
```

---

## 4. 技术选型建议

### 4.1 需要引入的依赖

| 方向 | 依赖 | 理由 | 替代方案 | 推荐 |
|---|---|---|---|---|
| 一 | `web-push` (Rust crate) | VAPID 密钥管理 + RFC 8030 HTTP 请求封装，避免手写加密 | 手动用 `reqwest` + AES-GCM，但 VAPID header 签名容易出错 | ✅ 使用 crate |
| 一 | Tauri v2 | 桌面壳，Rust 原生 + 现有 SPA | Electron（150MB+二进制）、Neutralinojs（更轻但生态小） | ✅ 推荐 Tauri v2 |
| 四 | esbuild | Go 二进制 bundler，零 Node 依赖 | Webpack/Vite（需要 Node + node_modules）、Rollup | ✅ 推荐 esbuild |
| 五 | `ffmpeg`（subprocess）| VOD 多 Session 合成 MP4 | `ffmpeg-next` crate（C 绑定）、纯 Rust MP4 muxer | ✅ 推荐 subprocess，原因见后 |
| 五 | `ical` (Rust crate) | .ics 日历文件生成 | 手动构建 iCalendar 字符串（~100 行） | 🔄 不需要 crate，手动即可 |

### 4.2 ffmpeg 子进程 vs Rust 绑定的决策

这是方向五中最有争议的技术选型。我的评估：

| 选项 | 优点 | 缺点 |
|---|---|---|
| **子进程 ffmpeg** | 成熟（20 年开发）、支持所有编解码格式、Docker 镜像可固定版本 | 进程管理（崩溃/超时/资源限制需自行实现）、子进程依赖系统环境 |
| `ffmpeg-next` crate | 进进程、Rust 式 API、error handling 绑定 | C API 安全风险（unsafe 包围的外部调用）、版本绑定、社区活跃度中等 |
| 纯 Rust TS mux | 无外部依赖、纯 safe Rust | 仅支持 TS→TS 合并，不支持 MP4 输出；纯 Rust MP4 muxer 不成熟（无人用） |

**推荐：子进程 ffmpeg + 信号量限制并发（如最多 2 个 ffmpeg 进程同时运行）+ 固定 Docker 镜像版本。** 接口层通过 `VodAssembly` trait 抽象，允许未来替换为纯 Rust 实现。不要采用 `ffmpeg-next`——它的 `unsafe` 边界涉及大量 C 指针操作，审计成本高。

### 4.3 前端 bundle 工具评估

当前前端零依赖的架构是一个优势（零构建时依赖、零运行时依赖），但已经达到其规模的极限。方向四需要引入 bundle 工具。评估标准：

| 标准 | esbuild | Vite | Webpack |
|---|---|---|---|
| 安装体积 | ~6MB (Go 二进制) | ~100MB (Node + deps) | ~200MB (Node + deps) |
| 配置复杂度 | 零配置（CLI 参数即可） | 需要 `vite.config.js` | 需要 `webpack.config.js` |
| `import/export` | ✅ | ✅ | ✅ |
| TypeScript | ✅ (esbuild 原生) | ✅ (esbuild transpile) | ✅ (ts-loader) |
| HMR | ✅ (esbuild 原生) | ✅ (Vite 核心能力) | ✅ (webpack-dev-server) |
| 构建时间（参考） | <50ms 增量 | <200ms 增量 | <1000ms 增量 |
| 与当前 `<script>` 标记的兼容 | 需要重构 import | 需要重构 import | 需要重构 import |

**推荐：esbuild。** 理由：不需要 `package.json`、不需要 `node_modules`、不需要插件配置。`esbuild index.js --bundle --outfile=dist/app.js --minify` 一行命令完成从零到 bundle 的过程。在 Docker 镜像中，esbuild 的 Go 二进制多阶段构建比 `npm install` 快 100 倍。

### 4.4 自建 vs 采购决策

| 组件 | 建议 | 理由 |
|---|---|---|
| CDN (HLS + blob) | **采购**（CloudFront / Cloudflare / Fastly） | CDN 是规模化后不可自建的基础设施，全球 PoP 覆盖是投入不经济的 |
| TURN 服务器 | **自建**（coturn + `aero-signaling` 的 IceServer 配置） | 每个区域一台 TURN 成本极低，采购 Twilio 等按带宽计费的 TURN 服务在大规模时成本失控 |
| Mobile push 证书 | **采购**（Apple $99/yr + Firebase 免费） | 这是平台托管费用，不能自建 |
| 实时媒体 SFU | **自建**（已完成 str0m SFU） | 已建的 str0m SFU 是核心差异化，不应采购替代品 |
| Web Push Service | **自建**（`web-push` crate + VAPID） | 零外部依赖，纯加密+HTTP POST 到 Mozilla/Google/Apple 的 Push Service，没有可采购的中间件 |

---

## 5. 实施路线图

### 5.1 优先级调整：对文档建议的修正

文档的优先级矩阵将方向五评为 🔥🔥🔥🔥🔥 产品影响力 + 极高竞争差异度。我**不完全同意**优先级排序中的"方向二→方向四→方向五"的顺序。

**我的修正排序：**

```
P0 (Week 1-6):  方向二 Bor/App 平台 MVP + 方向一 Web Push
P0.5 (Week 4-10): 方向四 前端基础设施重构 + 高优 UI 模块
P1 (Week 6-16): 方向五 虚拟活动 Phase 1-2
P2 (Month 4+):  方向一 Tauri + 方向三 多区域
```

**理由**：方向二和方向四互相增益——bot 平台的安装 UI 和管理面板会推动 `web/apps.js` 的创建，而前端基建的重构（esbuild + state layer）为方向二的 UI 提供了高质量的框架。方向五虽然产品价值最高，但其对 ffmpeg、Event Orchestrator 编排、多 Session 直播流的架构依赖意味着它更适合在方向二/四的初期成果上启动。

### 5.2 阶段划分和里程碑

#### Phase 1: Bot 平台 MVP + Web Push 落地（Week 1-6）

| Milestone | Week | 产出 | 验证标准 |
|---|---|---|---|
| M1 | W2 | `CommandRegistry` trait + `commands.rs` 重构 | 内置命令向后兼容，注册命令通过 bot delivery 回调 |
| M2 | W3 | `POST /api/apps` CRUD + install flow（无 OAuth） | 反向测试：注册 bot → 安装 → 发 slash 命令 → 收到回调 |
| M3 | W4 | Interactive component 回调 | Button 点击 → DB write + event + HTTP 回调 bot URL |
| M4 | W5 | Web Push 服务端 `WebPushGateway` | VAPID key gen → encrypt → push to Chrome → notification 抵达 |
| M5 | W6 | Web Push 客户端 SW | SW 注册 → 通知点击 → 打开正确 room deep link |

**风险点**：方向二的权限模型设计拖延。**缓解**：Phase 1 只做 workspace-level 完全信任（安装即拥有所有权限），Phase 2 加细粒度 scope。

#### Phase 2: 前端基础设施 + 高优 UI（Week 4-10，与 Phase 1 部分重叠）

| Milestone | Week | 产出 | 验证标准 |
|---|---|---|---|
| M6 | W5 | esbuild 集成 + `<script>` 标签替换为 bundle | 页面加载时间不变或降低，eslint 仍正常工作 |
| M7 | W7 | `state/` 模块拆分（messages/rooms/ui） | `context.js` 减少到 <50 行编排代码 |
| M8 | W9 | Canvas UI (`web/canvas.js`) + Tasks UI (`web/tasks.js`) | 端到端：创建 → 编辑 → 删除，无服务端错误 |
| M9 | W10 | 频道管理 UI 补全（retention/bookmarks/sections） | 所有 `channels_*` 路由对应的 UI 都存在 |

**风险点**：esbuild 的 `import` 语法与现有 `$(function(){...})` DOM ready 模式的兼容。**缓解**：逐步迁移，先让 esbuild 只 bundle 新模块，旧模块仍通过 `<script>` 加载，直到所有模块迁移完成。

#### Phase 3: 虚拟活动 Phase 1-2（Week 6-16）

| Milestone | Week | 产出 | 验证标准 |
|---|---|---|---|
| M10 | W8 | Event 实体迁移 + CRUD REST API | POST/GET/PATCH/DELETE 全绿 |
| M11 | W10 | EventSession 子表 + session 间自动切换 HLS | WHIP 推流结束 → 自动创建下个 session 的 HLS playlist |
| M12 | W13 | 注册/RSVP + .ics 下载 | 注册流程完整，ics 能被 Calendar 导入 |
| M13 | W16 | Q&A 面板（提交→mod→publish→标记已答） | 主持人 UI 验证三态流程 |

**风险点**：HLS playlist 切换的同步时序——观众在 session 切换时看到黑屏或错误。**缓解**：在 session 切换前 30 秒发 `StreamEvent::SessionSwitchPrepare` ，客户端预加载下个 session 的 `.m3u8`；切换 moment 发 `StreamEvent::SessionSwitch`，客户端 `hls.swapAudioCodec()`（hls.js 不直接支持，需 reload）。

### 5.3 风险矩阵与缓解策略

| 风险 | 级别 | 概率 | 影响 | 缓解 |
|---|---|---|---|---|
| Bot 权限模型过于宽松导致安全性事故 | 🔴 | 中 | 高 | Phase 1 限定内部 app（workspace token），Phase 2 引入 OAuth + scope，发布安全指南 |
| ffmpeg 子进程资源耗尽（内存/CPU） | 🟡 | 低 | 中 | `Semaphore` 限制并发 ffmpeg 实例（≤2），`tokio::process::Command` 设 timeout，Docker cgroup 限制 |
| 前端重构（esbuild + state layer）破坏现有功能 | 🟡 | 中 | 中 | 增量重构，每个 state/ 模块单独 PR 合入，旧 `context.js` getter 保持向后兼容垫片 |
| 方向五 HLS playlist 切换导致直播观众断流 | 🔴 | 中 | 高 | 客户端预加载 + session 切换前静帧保留 + 服务端保留切换前的 TS 片段直到切换稳定 |
| Tauri 壳中 Web SPA 的 notification API 重复触发 | 🟡 | 低 | 低 | Tauri 侧 drain WS 通知事件，使用 `app.emit` 频率限制（每秒最多 1 条原生通知） |
| NATS durable consumer 重连风暴（大量实例同时重连） | 🟡 | 低 | 中 | 启动时 jitter delay（`tokio::time::sleep(Duration::from_millis(rand))`），NATS server 端 `max_pending` 限制 |

### 5.4 不推荐的路径：应该明确排除的选项

1. **不要为方向二构建独立的 Bot SDK crate**。MVP 阶段 bot 只是一个 POST 到 webhook URL 的 HTTP 回调，不需要 SDK。Slack 的 Bolt SDK 是平台成熟后才建设的。

2. **不要为方向五引入 DAG 工作流引擎**（如 Temporal / Inngest）。Event 生命周期当前只有 ~6 个状态（Draft→Published→Started→Ended→Cancelled），状态机可以硬编码。等 Event 类型增加到 10+ 且有复杂分支条件时才考虑工作流引擎。

3. **不要为方向四一次性重写所有 28 个缺失 UI**。选 6-8 个高频率使用（画布、任务、审批、频道管理、创作者工具）先覆盖。其余在用户需求驱动下按需建设。

4. **不要为方向一同时启动 iOS + Android 原生开发**。这是最大的资源陷阱。先 Web Push + Tauri 覆盖桌面，再 Mobile SPA 响应式覆盖移动浏览器，等团队扩充后再启动原生开发。

---

## 总结

这份战略分析文档是我目前见过的最完整的 Aero IM 扩展方向分析。上述评估中，我对其优先级排序做了微调（方向二/四的依赖关系应更紧密地耦合在 Phase 1），对技术选型（esbuild 替代 Vite、子进程 ffmpeg 替代绑定）做了更具体的推荐，并对一些架构风险（ffmpeg 资源限制、方向五 HLS 切换的同步时序）做了展开。

**最关键的一句话**：这 5 个方向中，方向二的底层管道（`bot_dispatch` + `BotDelivery` + `webhook`）是复用率最高的架构资产——方向二用它做 bot 回调，方向五的 Event Orchestrator 用它做活动生命周期通知，方向一用它做推送触发的 bot 动作（如"推送按钮点击后发送消息"）。所以方向二的"MVP 不建新 crate、只加注册层"策略不仅降低了方向二自身风险，也为其他方向保留了底层管道的灵活性。
