# 架构分析报告：Aero IM 生产者端投递可靠性、WebSocket 优雅关停及系统性架构债务

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 的整体架构设计体现了**高水准的事件驱动分布式系统思维**：

- **分层清晰**：自下而上的 crate 依赖树（基础→IM→直播→组合）确保了编译期隔离和模块化；每个 crate 对应明确的能力域，无跨层环依赖。
- **总线中枢模式**：NATS JetStream 作为跨实例事实源 + Hub 作为进程内扇出，实现了线性可扩展的扇出模型。durable consumer 配合 at-least-once 语义为数据完整性提供了基线保障。
- **确定性可观测痕迹**：`MESSAGES_SENT_TOTAL` 作为唯一计数收口、全链路 Prometheus 指标、`x-request-id` 传播——这些在同类项目中往往缺失。
- **媒体 seam 隔离策略**：SFU、WHIP、SRT 等媒体组件各自有独立 crate、独立测试套件（虽未在生产接线），降低了核心业务 crate 的认知负载。
- **预算/限流体系**：AI 调用有双层 cost budget、消息路径有 per-ws rate limiter、SCIM 有幂等键——相比多数实时系统只在网关层做限流，这里的多层次限流设计是先进的。

### 1.2 核心架构债务（与评审文档重叠 + 新增）

评审文档的 5 个方向覆盖了架构债务的主要方面，但从架构视角，我将其重新组织为三组根本性债务：

#### 组 A：进程边界完整性债务（P0）

| 债务 | 表现 | 根本原因 | 与评审方向的关系 |
|------|------|---------|-----------------|
| **DB→NATS 双写无原子性** | 消息入库但未投递；NATS 已投但 DB 回滚 | `messages.insert()` 与 `publish_room_event()` 在异步上下文中串行执行，无事务协调器 | 方向一 |
| **关停不排空** | 滚动更新丢弃最后一批事件 | 关停流程未 Hook Hub 扇出 + WS 连接生命周期 | 方向二 |
| **at-least-once 补偿缺失** | `warn!` + `inc_counter` 是唯一的"补偿" | 设计假定 NATS publish 不会失败（或失败可忽略） | 方向一的延伸 |

这三个问题有一个**共同的根因**：**系统没有统一的事务边界管理策略**。PG 与 NATS 之间、扇出与连接生命周期之间、at-least-once 承诺与实际恢复代码之间——边界处都缺少形式化的保证机制。

#### 组 B：弹性边界缺失债务（P1-P2）

| 债务 | 表现 | 根本原因 |
|------|------|---------|
| **扇出无背压** | 高流量房间压垮慢消费者 | Hub 的 `try_send` 对慢消费者静默丢帧；直播弹幕与 IM 消息共用 unbounded 遍历 |
| **配置项零验证** | 拼写错误在启动后数分钟才暴露 | 无集中 config 校验层，figment 的 `Env` 层仅做 key-value 映射 |
| **协议版本化缺失** | 向前兼容由"不变"保障 | 未设计 API/WS 版本化方案，Block/RoomEvent 枚举通过"不加字段"保守演进 |

#### 组 C：体系性技术债（P3+）

- **无契约测试**：AI 审核路径 / moderation bot 的幂等性完全依赖集成测试（CI 不跑），模块边界无基于 property-based testing 或 contract testing。
- **媒体 seam 的测试债务**：`SfuMediaSession` / `call_bridge_supervisor` 零单元测试在 CI 运行，重构风险极高。
- **Web 客户端无类型契约**：WS `ServerFrame` 的 Rust 端与 `web/` 的 `ws.on('msg:...')` 之间无共享 schema 或类型生成——静默接收未识别帧在后端加新 variant 时不会触发编译错误，而是静默丢弃。

### 1.3 关键设计决策的合理性评估

| 决策 | 合理性 | 评估 |
|------|--------|------|
| NATS as event backbone | ✅ 正确 | JetStream 的 per-subject seq 正好解决 Real-time ordering；多 consumer group 可独立 checkpoint |
| PG commit → NATS publish (串行, 无协调) | ❌ 最大弱点 | 这不是"设计决策"而是"缺失决策"。串行双写在日志/分析/次要场景可接受，在**消息投递**（核心业务）路径上不可接受 |
| Hub `try_send` with bounded mpsc | ⚠️ 有代价的权衡 | 用静默丢帧换无阻塞——在 IM 场景可接受（客户端拉历史补帧），在直播场景有问题（弹幕不存 PG）。问题在于**两种场景共用一套扇出逻辑** |
| 按 per-subject seq + ack 保障 at-least-once | ✅ 正确但需要互补 | seq 用于客户端去重，但生产者崩溃导致 seq 空洞（乱序 NACK 被解释为"已该跳过的收不到的"）——需要 producer 侧补空，这在 IM 场景也重要（用户看到"消息已发送"但实际永不到达） |
| 媒体 seam 不接线 | ⚠️ 灰度策略 | 对大型项目合理——"先建骨架，再连血管"。但应设置明确的灰度标准（如"单节点 SFU 稳定 1 个月后再接线跨节点桥"），当前未见此类标准 |

---

## 2. 扩展方向

### 方向 A（新增）：发件箱模式（Outbox / Transactional Outbox）

**这是方向一修复方案，但作为架构扩展来设计才有价值。**

#### 为什么需要

当前 PG→NATS 双写无原子性，是**系统唯一在核心路径上可能导致永久的、业务可见的数据丢失**的故障模式。修复它不是"优化"，而是"止血"。

#### 核心挑战

1. **延迟-一致性权衡**：outbox relay 引入额外延迟（消息先写 outbox 表 → relay 读 → publish → delete）。用户期望"回车即见"，超过 200ms 的 UI 延迟会降低即时感。
2. **幂等消费**：outbox 行可能被 relay 处理两次（NATS 已发布但 ack 前崩溃），需要 `seq` + `ON CONFLICT DO NOTHING` 防重。
3. **与既有 AiWorker 预算系统的协调**：AiWorker 也是基于 `FOR UPDATE SKIP LOCKED` 轮询 PG——outbox relay 将是第二个类似的"PG 轮询消费者"。确保它们不竞争连接池且 mutual exclusion 正确。

#### 架构变更

```
当前：
  POST /messages → messages.insert() → publish_room_event() → 200 OK
  窗口：publish 失败 = 消息丢失

新（outbox）：
  POST /messages → messages.insert() + event_outbox.insert() (同一事务) → 200 OK
  OutboxRelay(后台): FOR UPDATE SKIP LOCKED → 按序 publish → ack → DELETE
                                  ↕ (双重保障)
  OutboxGC(定时器): 扫过时行(>5min) → alert + DELETE
```

- `event_outbox` 表结构：`id UUID, aggregate_type TEXT, aggregate_id UUID, event_type TEXT, payload JSONB, created_at TIMESTAMPTZ, attempt_count INT`。
- Relay 实现策略：**不要**用第二个 `tokio::spawn` 循环（与 AiWorker 重复模式），而是放在既有定时器框架内（`bin/boot/background.rs`）。间隔 100ms（不可配置，保持低延迟），每次 claim ≤20 行。
- **关键决策点**：relay 是否强制按 subject 顺序？强制（加 `ORDER BY created_at`）会限制吞吐但保证 seq 字典序；不强制则 NATS per-subject seq 可能乱序，客户端跳 seq 并发起 RESYNC。

**我的建议**：NATS 的 per-subject seq 已经保证了严格递增的 ordering guarantee，outbox relay **不需要**严格按 PG 时间顺序发布。`publish_room_event` 调用时 mint 的 seq 是单调的（`bus/seq.rs`），relay 只需按 seq 发布即可。

#### 对现有系统的影响

- 所有 `publish_room_event` 调用点（20 处）需要改为 `event_outbox.insert` + 移出 `publish_room_event` 调用，改为 relay 处理。**这是影响最大的部分**，需要逐点审计。
- migration 需要 `CREATE TABLE IF NOT EXISTS event_outbox`。
- 连接池压力略微增加（多 2-4 个 relay 连接 + outbox GC 定时器）。
- 消息发送延迟增加约 1-5ms（outbox INSERT 发生在 PG 事务内 vs NATS publish 的 RTT）。实际用户体验不变（WebSocket 扇出依赖 `run_bus_listener` → durable consumer，producer 侧延迟不是主要矛盾——扇出延迟才是）。

#### 实施选项

| 选项 | 延迟影响 | 实现成本 | 一致性保证 | 推荐 |
|------|---------|---------|-----------|------|
| **A: 完整的 outbox（INSERT + relay）** | +1-5ms PG 写入 | 3-4 人周 | 最强：事务级原子 | ✅ 推荐 |
| **B: NATS publish 失败重试 + fallback outbox** | 正常路径无变化 | 1-2 人周 | 弱：失败后才回写 outbox | ❌ 窗口仍然存在 |
| **C: NATS publish 放在 PG 事务前** | 无 | 0.5 人天 | 最弱：NATS OK 但 PG rollback | ❌ 错误方向 |

---

### 方向 B（新增）：扇出架构分层隔离

**评审方向四的架构化延伸。**

#### 为什么需要

当前 `hub.rs` 对所有事件类型和所有房间使用统一的扇出模型，导致：

- 直播弹幕的扇出延迟影响 IM 消息扇出（共享遍历锁）
- 慢消费者（网络差）背压影响同房间其他消费者
- 无 per-room 或 per-stream 的隔离调度

#### 核心挑战

1. **扇出拓扑选择**：per-room dedicated channel vs per-connection priority queue vs 共享 channel + weighted fair queuing。
2. **直播弹幕的"可丢性"设计**：弹幕可以丢，`run_live_bus_listener` 已经是 ephemeral consumer——扇出侧也应反映这一语义。但是目前 `hub.stream_watchers` 与 `hub.recipients` 使用相同的 `try_send` 模式。
3. **与 NATS consumer 的配合**：IM 用 durable consumer（at-least-once），直播用 ephemeral——扇出逻辑应该意识到上游的可靠性级别差异。

#### 架构变更

建议新增 `HubV2` 抽象（或逐步替换现有 `Hub`），引入两层调度：

```
HubV2
├── RoomLayer(IM) — per-room bounded channel → fan_out_raw
│   └── PerRoomChannel{capacity: 512, drop_policy: "backpressure"}
├── StreamLayer(live) — per-stream bounded channel → fan_out_raw
│   └── PerStreamChannel{capacity: 1024, drop_policy: "drop_oldest"}
└── PriorityLayer — 跨层按 event_type 的 priority weighting
```

- `IM` 消息走 `RoomLayer`：单个慢消费者不会影响其他房间，但同房间内慢消费者仍受影响（这是可接受的——房间内消息通常是完整的会话）。
- 直播弹幕走 `StreamLayer`：使用 `drop_oldest` 而非 `try_send` 的静默丢帧，让慢消费者看到"弹幕太快了自动降级"而非"消息丢了也不知道"。
- `PriorityLayer`：`CallEvent`（通话信令）优先级高于普通消息，`SystemEvent`（删除/禁言）高于普通消息——确保控制面不会被数据面饿死。

**注意**：这是一个大重构。建议先做**最低侵入的方案**：在当前 Hub 中增加 `event_priority` 参数的 `fan_out_raw` 重载，使 `CallEvent` 等控制面走独立 mpsc 而非常规消息通道。

#### 对现有系统的影响

- `hub.rs` 需要大幅改动，但 `Hub` 的对外接口 `fan_out_raw(&self, ...)` 可以保持向后兼容（内部重构成新结构）。
- `run_bus_listener` 和 `run_live_bus_listener` 已有的 `hub.fan_out_raw` 调用无需改动。
- 所有实时帧处理路径需要指定 `event_priority`（添加 `EventPriority` enum）。
- 测试覆盖需要同步更新——当前 hub 测试较少，重构后应新增 per-layer 的隔离测试。

#### 实施选项

| 选项 | 复杂度 | 风险 | 推荐 |
|------|--------|------|------|
| A: 完全 HubV2 重构 | 高（2-3 周） | 中 | 长期目标 |
| B: 只在当前 Hub 加 `event_priority` 参数 | 低（3-5 天） | 低 | ✅ 短期最优 |
| C: 直播和 IM 拆为两个独立 Hub | 中（1-2 周） | 中 | 候选 |

---

### 方向 C（新增）：优雅关停的体系化设计

**将评审方向二提升为体系化设计。**

#### 为什么需要

当前关停流程是"发 SIGTERM → 等一会儿 → 硬杀"：

```
shutdown.rs::shutdown_signal:
  shutting_down = true
  tokio::time::sleep(Duration::from_secs(10))
  ai_shutdown.cancel()
  // 没有：Hub draining / WS draining / bus listener drain
```

Kubernetes 滚动更新要求 Pod 在 `terminationGracePeriodSeconds`（通常 30s）内完成 draining。当前设计在滚动更新期间会丢失最后一组扇出事件和 WS 帧。

#### 核心挑战

1. **确定性关停顺序**：关停流程需要是一个**有向无环图**，每个步骤等待前一步完成：
   ```
   1. 从上游负载均衡器注销自身
   2. 停止接受新 WS 连接（Axum 层）
   3. 停止 NATS consumer（unsubscribe durable/durable）
   4. Hub draining：排空 mpsc 中剩余帧
   5. WS draining：对每个连接发送 GOING_AWAY 帧，等待 flush
   6. 关闭 NATS 连接
   7. 关闭 PG/Redis 连接池
   8. 退出
   ```
2. **时序窗口匹配**：K8s 的 PreStop hook 和 terminationGracePeriodSeconds 需要与服务内 draining 配置一致。如果 PreStop 只 sleep 5s 但内部 draining 需要 15s，窗口不对齐。
3. **WS 帧的 GOING_AWAY 与 resequence**：WS 关停帧告诉客户端"准备重连"，客户端应带着 `since` cursor 重连，服务端投递其拉下的帧。当前 Web 端没有此逻辑。

#### 预期架构变更

- `shutdown.rs` 需要在 `ai_shutdown` 基础上新增 `hub_shutdown`（控制 Hub draining）和 `ws_shutdown`（控制 WS draining）。
- `Hub` 新增 `drain(duration)` 方法：关闭 `run_bus_listener` / `run_live_bus_listener` 的扇入（stop consuming），然后等待 bounded mpsc 排空（或 timeout），最后关闭所有 recipient channel。
- `handle_socket` 需要监听 `ws_shutdown` 事件，收到后发送 WS Close frame (status=1001, reason="going_away") 并停止消费新消息。
- `hub.rs` 需要维护活跃 WS 连接数，`drain` 方法等待连接数归零（或超时）。

#### 可选方案

| 方案 | 优势 | 劣势 | 推荐 |
|------|------|------|------|
| **A: 仅 Hub draining（无 WS 关停帧）** | 实现最简单（2-3天），修复主要痛点 | 客户端感知不到关停，只能通过重连超时触发 | ⚠️ 最低可接受方案 |
| **B: Hub draining + WS GOING_AWAY** | 客户端及时感知，可立即重连 | 需要 Web 端配合修改（检测 1001 帧触发重连） | ✅ 推荐 |
| **C: 完整 K8s-native 生命周期（PreStop + readinessProbe draining + Pod Disruption Budget）** | K8s 原生集成，零丢帧保证 | 需要运维层配合，实现复杂（≥2周） | 远期目标 |

**注**：评审文档中建议将方向二从 P0 降为 P1。从架构影响面看，方向二在单实例部署时是 P2，在 K8s 滚动更新场景是 **P1+**。鉴于 README 和 AGENTS 都体现系统部署在容器/可扩缩环境中，我建议保留为 P1。

---

### 方向 D（新增）：配置体系的形式化验证

**评审方向三的架构深化。**

#### 为什么需要

~40 个配置项零前置校验，当前结构：

```
[figment::Env 层] → [无校验中间层] → [各模块 env::var()]
```

问题不只是"拼写错误"（运行时暴露），更深层的是：

- **类型安全缺失**：`AERO__SERVER__BLOB_DIR` 作为一个 path，在作为 `PathBuf::new` 使用前可能不存在/不可写——但失败点远离配置点。
- **依赖关系缺失**：`AERO_AI_MODERATION` 依赖 `AERO_AI_*`（Anthropic key），但依赖关系不在配置层表达——A 配了 B 没配导致半初始化状态。
- **默认值零散**：`AERO__SERVER__RETENTION_SWEEP_SECS` 默认 3600，但默认值定义在 `bin/boot/background.rs` 而不是配置层。要改默认值得翻 5 个文件。

#### 架构变更

建议引入 `ConfigValidator` 层，在 boot 时统一校验：

```
ConfigValidator:
  - 校验每个已知 key 的格式/类型/存在性/有效性
  - 校验 key 间的依赖关系（A => B must_exist）
  - 校验路径存在性（Path::exists / 可写检查）
  - 校验范围（端口范围、连接数范围）
  - 所有校验失败一次性报告，不 fail-fast on first error
  - 新增 --validate-config 只校验不启动

依赖关系表（示例）：
  AERO_AI_MODERATION       → AERO_AI_ANTHROPIC_KEY (must_exist)
  AERO_S3_BUCKET           → AERO_S3_ACCESS_KEY + AERO_S3_SECRET_KEY (all_or_nothing)
  AERO__SERVER__HLS_DIR    → AERO__SERVER__BLOB_DIR (same_fs 可选)
  AERO__SERVER__BLOB_DIR   → Path::exists + is_writable
```

**设计原则**：

- **不引入新依赖**（不用 `validator` crate 或 `serde_valid`，保持最小依赖）。校验逻辑用纯 Rust struct + `Display` trait 实现。
- **统一的失败报告**：收集所有 error，一次性打印到 stderr 再 exit(1)。不要 fail-fast 逐个报错逐个 exit——用户需要一次性看到所有配置问题。
- **配置审计输出**：成功启动时 `info!("Config validated: {} checks passed", n)`，内容不走标准输出（避免干扰 JSON log）。

#### 对现有系统的影响

- 零运行时影响（校验在 server 启动前完成）。
- 需要 `ConfigValidator` struct + 每个 crate 提供自己的 `validate()` 函数签名。每个 crate 需要导出 `fn validate_config(cfg: &Config) -> Vec<ValidationError>`。
- 对现有代码零侵入：校验层只是读取配置结构体字段，不修改模块内部的 `env::var()` 使用方式（但建议长期逐步迁移到统一 config struct）。

---

### 方向 E（新增）：协议版本化的渐进策略

**评审方向五——最有价值的长期方向。**

#### 为什么需要

当前无版本化设计意味着：

- REST 路径永远是 `/api/rooms/:id/messages`，无法同时支持 v1 和 v2。
- WS 帧 `kind` tag 的变化（加新 variant）会静默破坏老客户端。
- `Block` 枚举的加字段（如加 `Block::Edited`）对于未更新的 web 端，表现为 `match` 走 `unknown` 分支——如果前端 `unknown` 分支 `console.warn` 可接受，但如果分支 `break` 了整个渲染管线则不可接受。

#### 核心挑战

1. **向后兼容 vs 快速演进**：版本化意味着多版本维护成本。在只有单一 web 客户端的情况下，版本化的 ROI 低——但一旦有第三方客户端或移动端 SDK，版本化就是必需品。
2. **枚举的演化契约**：Rust `enum` 是 tagged union，加 variant 会 break exhaustive match。需要明确"哪些 enum 是公开协议契约，哪些是内部实现细节"。

#### 架构变更

**分层版本化**：

```
REST API
├── /api/v1/ → 当前所有路由（冻结 "frozen"）
├── /api/v2/ → 新路由（兼容 v1 模式，逐步接入）
└── /api/internal/ → 实例间通信（不支持外部，无需版本化）

WS 协议
├── 握手时 client 声明 version (wsParams.version)
├── 服务端响应 server_version + capabilities[]
└── 帧定义：所有公开帧加 __protocol_version 字段或 namespace prefix
```

**枚举演化契约**（建议）:

| 枚举 | 公开/内部 | 兼容规则 |
|------|----------|---------|
| `RoomEvent` | 公开 | 永远 `#[non_exhaustive]`；加 variant 先 deprecate 再删除（半年窗口） |
| `Block` | 公开 | 同上；加字段 `#[serde(deny_unknown_fields)]` |
| `ClientFrame` | 公开 | 同上 |
| `ServerFrame` | 公开 | 同上 |
| `StreamEvent` | 公开 | 同 `RoomEvent` |
| `PresenceState` | 内部 | 可任意变更 |

**注意**：`#[serde(deny_unknown_fields)]` 对于接收方是向后兼容的（老客户端不会发新字段），但对于发送方是破坏性的（新客户端用新字段发老服务端会报错）。建议只在 `Block` 等关键枚举上加，且配合版本协商使用。

#### 对现有系统的影响

- 所有路由需要加 `/v1/` 前缀——这是 break change，需要前后端同步上线。
- WS 握手协议需要支持 version 参数。
- Rust 端的 `non_exhaustive` 标注需要加，确保新 variant 被老 match 编译时产生显式警告。
- Web 端需要处理 `server_version`，对不同版本做不同的帧解析。
- **这是影响面最大的变更**（影响 REST、WS、Rust 枚举定义、Web 端解析），建议作为 P2 渐进推行而非一次性大重构。

---

## 3. 接口设计建议

### 3.1 核心原则

| 原则 | 说明 | 对应债务 |
|------|------|---------|
| **边界处形式化契约** | 每个模块边界（PG↔NATS、Hub↔WS、SFU↔Bridge）需要明确的成功/失败/超时/重试协议 | 方向一、二 |
| **背压是第一公民** | 扇出路径必须能向发送方传递背压信号，不能通过静默丢帧"隐藏"问题 | 方向四 |
| **配置即契约** | 所有配置项的合法性、依赖关系和默认值在统一层面声明 | 方向三 |
| **协议版本显式化** | 任何跨进程/跨语言的通信协议必须有线缆上的版本标识 | 方向五 |

### 3.2 关键接口改进建议

#### Hub 接口

**当前**：

```rust
pub fn fan_out_raw(
    &self,
    recipients: Vec<UserId>,
    msg: ServerFrame,
) -> Result<(), HubError>
```

**问题**：无优先级、无背压、无 room/stream 隔离。

**建议演进（分两步）**：

第一步（1-2 周）——加优化参数不做结构性改变：

```rust
pub fn fan_out_raw(
    &self,
    recipients: Vec<UserId>,
    msg: ServerFrame,
    priority: EventPriority,      // 新增
    source: EventSource,           // 新增：Room / Stream / System
) -> Result<(), HubError>
```

第二步（长期）——重构 Hub 为分层结构，但对外保持上面签名兼容。

#### Bus（EventBus trait）

**当前**：

```rust
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish_bytes(
        &self,
        subject: &str,
        payload: &[u8],
    ) -> Result<(), BusError>;
    
    async fn subscribe(
        &self,
        subject: &str,
        queue_group: Option<&str>,
    ) -> Result<Box<dyn Stream<Item = BusMessage> + Unpin>, BusError>;
}
```

**问题**：
- `publish_bytes` 返回 `Result<(), BusError>`，但成功仅意味着"已发送给 NATS 服务"，不保证"已持久化"（实际代码用了 `ack.await`，但 trait 签名不体现）。
- 不可插拔：无法在不改动 trait 的情况下引入 outbox 模式。

**建议**：

```rust
#[async_trait]
pub trait EventBus: Send + Sync {
    /// 发布且等待持久化确认。
    /// 返回 PersistenceConfirmed 与 PublishedAndAcked 两种状态。
    async fn publish_durable(
        &self,
        subject: &str,
        payload: &[u8],
    ) -> Result<PublishResult, BusError>;
}

pub enum PublishResult {
    /// NATS 确认已持久化（包括 replicated quorum）。
    PersistenceConfirmed { seq: u64 },  
    /// 已写入本地 outbox 表，将由后台 relay 投递。
    PublishedViaOutbox { outbox_id: Uuid },
}
```

关键：返回 enum 让调用方明确知道持久化完成了还是退而求其次走了 outbox。

#### WsHandler / WsSender 接口

**建议新增**：

```rust
pub enum WsCloseReason {
    GoingAway,          // 服务端关停/滚动更新
    SessionExpired,     // token 过期
    Kicked,             // 被踢下线
    AdminTerminated,    // 管理员强制断开
}

impl WsSender {
    /// 发送关闭帧并等待客户端确认（或 timeout）。
    pub async fn graceful_shutdown(reason: WsCloseReason, timeout: Duration) -> Result<()>;
    
    /// 发送延迟指标帧（支持客户端侧速度调节）。
    pub async fn send_congestion_warning(backoff_ms: u64) -> Result<()>;
}
```

### 3.3 是否需要新的抽象层

**是**，需要三个：

1. **配置治理层**（`ConfigLayer`）：统一 config struct + validate + audit。位于 boot 时序的最前端。
2. **投递保证层**（`DeliveryGuaranteeLayer`）：outbox relay + retry + DLQ。在 `EventBus` 和 `ImService`/`live.rs` 之间。
3. **连接生命周期层**（`ConnectionLifecycleLayer`）：WS 连接注册 → draining → 关停帧 → 重连引导。在 `Hub` 和 `handle_socket` 之间。

这三个抽象层与现有 crate 的关系：

```
现有 crate      新抽象层
aero-bus  ───  DeliveryGuaranteeLayer (outbox relay)
aero-server ──  ConfigLayer (validate before boot)
ws/ws_impl ──  ConnectionLifecycleLayer (draining + graceful shutdown)
```

---

## 4. 技术选型

### 4.1 是否需要引入新技术栈

**不建议**引入新的核心基础设施依赖。理由：

| 场景 | 可选方案 | 评估 |
|------|---------|------|
| Outbox relay | 已有 `AiWorker` 模式（`FOR UPDATE SKIP LOCKED`）可复用 | ✅ 零新依赖 |
| 配置验证 | 纯 Rust struct + `Display` trait | ✅ 零新依赖 |
| Hub 分层 | 已有 `mpsc` 和 `HashMap` 可用 | ✅ 零新依赖 |
| WS versioning | 握手协议字段扩展 | ✅ 零新依赖 |
| 协议 schema generation | `schemars` + `ts-rs` | ⚠️ 推荐引入，但可推迟 |

唯一推荐引入的轻量依赖：

| 依赖 | 用途 | 替代方案 | 推荐理由 |
|------|------|---------|---------|
| **schemars** (已审查, 0.8) | Rust struct → JSON Schema | 手写 schema 文档 | 自动生成协议契约，与 `ts-rs` 配合生成 TypeScript 类型，消除 web 端的静默类型不匹配 |
| **tokio-util::CancellationToken** (已在用) | 关停编排 | 自定义 `AtomicBool` + event | 已在用，但未正确连接到 Hub 和 WS draining——这不是"新引入"而是"正确使用已有工具" |

**审慎排除**：

| 候选 | 排除理由 |
|------|---------|
| **Debezium / PostgreSQL Logical Replication** | 为 outbox relay 引入 CDC 基础设施过于重型（需要 wal2json / pgoutput 插件，运维复杂度大增）。PG 轮询 + `SKIP LOCKED` 在每秒数百事件下足够 |
| **gRPC / Cap'n Proto** | Aero IM 现在走 JSON over NATS + JSON over WS——增加序列化层只在 RTP 级别是合理的，在应用层不必要 |
| **Kubernetes operator** | 过于面向运维。优雅关停的 draining 应该在应用层处理，而不是依赖外部编排 |
| **Envoy / Linkerd sidecar** | WS 层的 graceful shutdown 应该在应用层做，envoy 的 connection draining 不能解决应用层关停帧问题 |

### 4.2 自建 vs 采购/复用

评审文档中未涉及采购决策。从架构角度：

| 组件 | 方案 | 理由 |
|------|------|------|
| Outbox relay | **自建**（复用 AiWorker 模式） | 业务逻辑高度绑定（需要 per-subject seq 维护、与 AiWorker 竞争连接池需协调） |
| 配置验证 | **自建** | 领域特定：需要校验 path 可写、key 依赖关系、端口范围——通用 validator 无法表达 |
| WS draining | **自建** | 完全绑定到 `hub.rs` + `handle_socket` + `WebSocket` 的内部状态 |
| 协议 versioning | **自建**（但可参考 OpenAPI / GraphQL federation 模式） | 不使用 OpenAPI 直接（太重），但学习其"向后兼容检查"方法论 |

核心结论：**零采购，全部自建，但复用既有模式**（`SKIP LOCKED`、CancellationToken、分层抽象）。

---

## 5. 实施路线图

### 5.1 优先级画像

二维矩阵：**破坏性（surface）** vs **影响面（scope）**

```
              破坏性高（数据丢失/不一致）
              │
    方向一     │ ● (P0) outbox
    方向二     │ ● (P1) WS draining
              │
影响面窄──────┼────── 影响面广
              │
    方向三     │ ● (P1) config validation
    方向四     │ ● (P1) fanout pressure
    方向五     │ ● (P2) api versioning
              │
              破坏性低（使用体验/运维）
```

### 5.2 阶段划分

#### Phase 0 — 马上能做（1-2 天）

- **方向三（Config validation）**：成本最低，风险最低，独立于所有其他方向。无代码入侵，仅是 boot 时序的前置。
- **方向一最小修复（NACK 写入 outbox）**：在 `publish_room_event` 的 `Err` 分支增加 outbox insert（PG fallback），不改变成功路径。这是最低风险的止血方案。

| 任务 | 工作估算 | 输出 |
|------|---------|------|
| 实现 `ConfigValidator` + `--validate-config` | 1 天 | 启动前全配置校验 |
| `publish_room_event` 失败时写 `event_outbox` + 简单 relay | 2-3 天 | 崩溃窗口缩小 90%（true outbox 仍缺） |

#### Phase 1 — 债务优先（2-3 周）

- **方向一完整 outbox**：所有 20 个调用点改为 outbox INSERT + relay publish。
- **方向二最小可行 draining**：Hub draining + WS 关停帧（选项 A+B），但不做 K8s 原生集成。

| 任务 | 工作估算 | 前置依赖 |
|------|---------|---------|
| `event_outbox` 表迁移 + 20 个调用点逐点修改 | 5-7 天 | Phase 0 relay |
| `hub.drain()` 实现 | 2-3 天 | 无 |
| `handle_socket` 添加关停帧 | 2 天 | hub.drain |
| Web 端 `ws.onclose(1001)` 重连逻辑 | 1 天 | 关停帧 |

#### Phase 2 — 弹性提升（2-3 周）

- **方向四扇出压力管理**：`EventPriority` + per-layer channel（选项 B 渐进）。
- **方向一与 AiWorker 的协调**：outbox relay 与 AiWorker 共享连接池的 mutual exclusion 策略。

| 任务 | 工作估算 | 前置依赖 |
|------|---------|---------|
| `EventPriority` 定义 + `fan_out_raw` 重载 | 2 天 | 无 |
| per-room `mpsc` 隔离 | 3-5 天 | EventPriority |
| outbox relay - AiWorker 连接池协调 | 1 天 | Phase 1 relay |

#### Phase 3 — 长期工程（1-2 月）

- **方向五协议版本化**：REST `/api/v1/` → `/api/v2/` + WS version 握手 + 枚举 `#[non_exhaustive]`。
- **方向二 K8s 原生集成**：PreStop hook + readinessProbe draining 信号 + PDB。
- **方向四完整 HubV2**。

| 任务 | 工作估算 | 前置依赖 |
|------|---------|---------|
| REST `/api/v1/` 路由迁移 | 3-5 天 | 前后端同步上线 |
| WS version 握手协议 | 2-3 天 | 枚举 `non_exhaustive` |
| `Block`/`RoomEvent` `deny_unknown_fields` + `non_exhaustive` | 1-2 天 | WS version |
| HubV2 完整实现 | 5-10 天 | Phase 2 per-room isolation |
| K8s PreStop + PDB | 2 天 | Phase 1 draining |

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| Outbox relay 导致 AiWorker 竞争连接池 | 中 | 高（连接池 starvation） | 独立 connection pool（`PgPoolOptions::new().min_connections(2).max_connections(4)`），不与 AiWorker 共享 |
| WS draining 引入新的 panic/crasher 路径 | 低 | 中 | draining 逻辑只接触 `Option.take()` 获取所有权，不使用 Rc/Weak。在 drain 路径加 `#[cfg(test)]` 测试覆盖 |
| REST `/api/v1/` 迁移 break SPA 或 mobile | 高（如果没有双版本并行） | 高 | 必须做双版本并行至少 1 个 release 周期：`/api/` 保持 v1 行为，`/api/v2/` 走新逻辑 |
| enumeration 合约变更在编译时不可观察 | 中 | 中 | 引入 `cargo test --workspace` + `cargo clippy -- -D clippy::match_wildcard_for_single_variants` |
| Outbox relay 延迟 vs NATS direct publish 延迟差异 | 低 | 低 | 在 relay 路径首行加 `metrics::histogram!("outbox_relay_latency")`，设定 SLO ≤ 50ms P99 |
| 关停 timeout 不当导致 SIGKILL | 中 | 中 | K8s `terminationGracePeriodSeconds` 设置为 max(drain_timeout + 5s, 30s)；应用层 drain 超时写 metric 报警 |

### 5.4 里程碑建议

```
M0 [1 周后]  — Phase 0 完成
               ✓ ConfigValidator --validate-config
               ✓ event_outbox fallback（nack 回写）
               ✓ 修正评审文档的两个事实错误并归档到 docs/analysis/

M1 [1 月后]  — Phase 1 完成
               ✓ 完整 outbox + relay（20/20 调用点）
               ✓ Hub draining + WS GOING_AWAY 帧
               ✓ Web 端重连引导

M2 [2 月后]  — Phase 2 完成
               ✓ EventPriority 分层扇出
               ✓ per-room/per-stream 隔离
               ✓ outbox relay 稳定性（P99 ≤ 50ms）

M3 [3 月后]  — Phase 3 完成（可选）
               ✓ /api/v1/ + /api/v2/ 双版本
               ✓ WS version 握手
               ✓ K8s 原生 draining
```

---

## 6. 对评审文档的总评

### 6.1 最认可的部分

1. **方向五的零重叠分析**：WS 协议版本化 + 枚举演化契约 + 能力协商确实未被既有文档覆盖，且是 Aero IM 长期可扩展性最被低估的债务。这是评审文档中**架构洞察最深的部分**。

2. **方向二的 Hub 扇出排空分析**：关停流程中 `run_bus_listener` 不被 `ai_shutdown` 控制 + `try_send` 在 `Closed` 状态下的静默行为——这个组合确实是当前代码中最容易被忽视的窗口。能追踪到这个路径表明代码阅读细致。

3. **方向一的 crash 时间线分解**：按 `messages.insert()` → `publish_room_event()` → crash 的时序拆解比"双写可能丢失"的概括有了质的提升。

### 6.2 需要修正的核心问题

4. **"fire-and-forget"表述不准确**：评审文档的正确性声明已经被代码验证驳回了。建议修正为"ack-wait pattern 正确，失败处理缺失"，这在架构层面是不同的结论——前者的修复是"改成 publish_with_ack"，后者是"加补偿机制"。

5. **与既有文档的重叠声明**：`scale-edge-security-analysis.md:76` 明确指向 producer 侧 outbox 方案，评审文档声称其聚焦于 consumer 侧补偿是误读。这影响了方向一的"原创性"定义——应重新定性为"深化而非发现"。

### 6.3 缺失的架构角度

评审文档聚焦于**5 个离散的代码级验证方向**，缺少以下架构层面的系统视角：

- **数据流端到端的延迟 SLO**：当前文档没有定义消息从 POST 到 WS 扇出的 SLO（既是架构需求也是可观测的关键输入）。没有 SLO 就无法判断 outbox 模式引入的延迟是否可接受。
- **容错模式的系统性缺失**：at-least-once 的补全不应只靠 outbox（方向一），还需要将"写入即为承诺"的意识贯穿到所有副作用路径（push bot、webhook、golive_bot）。
- **可测试性债务**：评审文档未将零 CI 媒体 seam 测试 + 缺失的 contract test 归类为架构债务。

### 6.4 最后建议

1. **v2 修正**：修正 fire-and-forget 表述 + scale-edge-security-analysis 重叠声明。
2. **归档路径**：移动到 `docs/analysis/2026-07-12-producer-edge-config-gaps.md`（评审文档声称后续引用来源，将其置于 analysis/ 下使引用路径语义正确）。
3. **作为"深化"时价值最高**：如果 v2 以"在已有 outbox 提议基础上补充代码级证据和扩展选项"为框架而非"全新发现"，文档的工程价值大幅提升。
