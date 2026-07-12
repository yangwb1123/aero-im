# 架构分析报告：Aero IM 系统

## 1. 架构评估

### 1.1 当前架构的优势

Aero IM 的架构设计在几个关键维度上展现了高质量：

**事件驱动骨架（Event DAG）** ——这是系统最坚实的架构决策。NATS JetStream 作为跨实例事实源，配合进程内 `Hub` 扇出，构成了一个清晰、可水平扩展的实时架构。三事件命名空间（`im.room.*` / `live.stream.*` / 通话信令）分离干净，避免了广播风暴混入同一 subject 空间。per-subject 单调 seq 机制（`bus/seq.rs`）为客户端去重/排序提供了坚实契约——这是很多 chat 系统缺失的关键设计。

**Crate 边界清晰**——从 `aero-common`（叶子）到 `aero-server`（组合），分层严格反对成环。每一层定义明确的职责范围，不越界依赖。基础层（common/bus/storage/auth）与业务层（im-core/live-core）与组合层（server）的分离，使得单元测试的范围清晰——你可以单独测 `ImService` 而不启动 WebSocket 或 HTTP 服务器。

**Seam 战术部署**——报告中对方向五的屏幕共享误判反而揭示了一个有趣的事实：屏幕共享完整实现 + 实时摄入/转码/推流框架完备 + call-bridge 控制面已建，说明了架构允许各功能独立演进。某个能力域（屏幕共享）可以提前完成，而不阻塞其他部分的联调。

**智能体模式的成熟运用**——§2 中的 bot/worker/timer 体系是事件驱动架构的自然延伸。关键在于每个智能体有明确的模块、触发条件、关键不变量和 fail-open 策略。这种模式让功能可以以正交方式叠加。

### 1.2 关键设计决策评估

| 决策 | 评价 | 理由 |
|------|------|------|
| NATS JetStream + Hub 双级扇出 | ✅ **正确** | 跨实例 durable + 进程内 bounded mpsc，避免单一消息总线成为扇出瓶颈；`try_send` 设计让扇出不影响消息消费 |
| Redis sorted-set 做集群状态 | ✅ **正确** | Presence/roster 的 TTL 驱逐天然适合 sorted-set；避免单进程内存状态扩散问题（AGENTS.md §1 明确"绝不靠单进程内存"） |
| 事件 tag `kind` 的 serde rename 策略 | ✅ **必要但易错** | 这是一种"已知漏洞"的模式规避方案。每添加新 variant，开发人员必须记得检查字段名为 `kind` 的冲突——会在运行时 panic，编译期不报错 |
| WS 状态全进程内存（Web SPA） | ❌ **架构债务** | 这是从"联调工具"到"生产产品"之间最大的缺口。整个 SPA 的状态模型假定页面永不刷新、用户永不关闭标签页 |
| SFU 路由用 `Arc<RwLock<HashMap>>` 而非 Redis | ✅ **合理决策** | SFU 操作频繁（每帧都要查路由表），走 Redis 会变成性能瓶颈。但代价是跨节点媒体需要额外的 call-bridge 机制来协调 |
| 输入校验仅后端、前端不镜像 | ❌ **UX 债务** | 后端校验是安全基线，但前端不校验导致用户发送超长消息后才知道失败，这是 UX 不可接受的 |
| at-least-once 无全局幂等框架 | ⚠️ **未来问题** | 目前各 bot 各自实现幂等（ooo_bot 用 `ON CONFLICT DO NOTHING`，moderation_bot 用 soft-delete 幂等），没有统一的幂等键框架。随着更多 bot/worker 接入，这会出现一致性问题 |

### 1.3 架构债务和技术债

按影响面排序：

1. **P0 — Web SPA 进程内存状态模型**：这是从原型/联调工具到生产产品最大的架构债务。整个前端的架构假设与生产需求（用户会关闭标签页、会在多设备间切换、会收到通知推送并期望在正确上下文中打开）完全冲突。

2. **P0 — 缺失前端路由框架**：与状态持久化直接关联。没有 URL 路由意味着：
   - 通知推送无法生成 deep-link
   - 用户在浏览器中前进/后退按钮的行为不可预期
   - 无法通过 URL 分享某个消息或房间
   - 页面刷新后用户丢失所有上下文

3. **P1 — Hub 扇出的锁竞争模型**：虽然不是性能瓶颈（感谢 `try_send` 和 bounded channel），但锁的粒度和模式不够优化。DashMap 的 per-shard 锁避免了全局竞争，但 `get_mut(pid)` 在单个用户多设备场景下造成串行化——如果用户有 5 台设备，五条并行消息需要按 pid 排队写锁。这个问题在单用户多设备场景下比大房间更严重。

4. **P1 — 前端无错误恢复策略**：websocket 断线→消息永远 pending、"静默 catch"覆盖 5 处路径、WS send 失败无声——这不是 UI 层次的问题，而是架构层次缺少"离线可靠"或"乐观更新+确认超时"的设计模式。

5. **P2 — 实时媒体路由与通话状态的非对齐**：SFU router（`SfuRouter`，进程内 HashMap）和 Hub 的 `call_rosters` 各自维护通话名册但有不同用途。AGENTS.md 提醒"**别和 Hub 的 call_rosters 混**"，但没有明确定义两者的一致性契约——在什么条件下 SFU 路由表的状态应该被视为通话的权威视图？在 `CallEgress` 跨节点使用哪个 roster 来决定路由？

### 1.4 架构成熟度总评

Aero IM 的后端架构（Rust 层）处于 **良好的 early-production 阶段**：事件骨架坚固、分层清晰、测试覆盖尚可。前端（Web SPA）处于 **prototype-to-product 过渡阶段**——核心功能可在受控环境中演示，但缺少作为生产产品所需的弹性和状态管理。

| 维度 | 后端 | Web 前端 |
|------|------|---------|
| 架构清晰度 | ⭐⭐⭐⭐⭐ | ⭐⭐ |
| 可扩展性 | ⭐⭐⭐⭐ | ⭐ |
| 错误处理/弹性 | ⭐⭐⭐ | ⭐ |
| 可测试性 | ⭐⭐⭐⭐ | ⭐⭐ |
| 部署回弹性 | ⭐⭐⭐⭐ | N/A |

这不是一个"后端做得差"或"前端做得烂"的问题——这是一个**架构投资倾斜的问题**：团队显然把核心架构精力投放在了 Rust 后端的事件模型和媒体管道上，前端的架构投资暂为"最小可行"。现在到了需要平衡投资的时候。

---

## 2. 扩展方向

### 方向 A：前端状态层重构（P0 — 立即开始）

#### 为什么需要

这不是一个功能需求——这是一个**架构迁移**。当前进程内存 state 的架构假设在一次页面刷新后就被打破。随着更多功能添加（多设备、通知、线程搜索），状态管理的复杂度指数级增长，当前模式无法维持。

从业务视角：用户在实际工作中会频繁切换标签页、重启浏览器、在不同设备间切换。如果每次都需要重新导航到正确上下文（甚至丢失未发送的消息草稿），产品无法在竞争中立足。

#### 核心挑战

- **状态持久化到 localStorage 不完全可行**：消息历史可能很大（数万条），localStorage 的 5-10MB 限制无法容纳完整的 `messagesByRoom`。需要分级策略：活跃房间的最近消息在内存 + 部分状态在 localStorage + 大部分状态从 API 恢复。
- **IndexedDB 的学习成本**：IndexedDB 的 API 基于请求/事务，与传统 JS 状态管理差异大。需要封装一层（如 Dexie.js 或 idb-wrapper 库）。
- **断线恢复的时序问题**：WebSocket 重连后，如何确定哪些状态已过时、哪些状态仍然有效？这需要前端有一个"版本向量"或"游标"机制来和服务端校准。

#### 预期的架构变更

```
当前状态:
State (进程内存) ← WS 事件流 → 服务端

重构后的分层:
State Layer (IndexedDB + 内存 LRU)
  ├── RoomState (当前活跃房间: 内存)
  ├── RecentState (最近 10 个房间: localStorage)
  └── HistoricalState (其余: IndexedDB)
        ↑
WS 事件流 + 恢复 API
        ↓
  服务端
```

- 引入 `StateManager` 类（或模块），作为所有状态读写的单一入口
- `StateManager` 内部实现 LRU 缓存策略：活跃房间保持全量在内存、最近关闭的房间序列化到 localStorage、其余状态在 IndexedDB
- 添加 `serializeSnapshot()` / `deserializeSnapshot()` 用于页面生命周期中的状态快速保存/恢复
- WebSocket 重连恢复协议：连接恢复后，客户端发送 `state_version` 或 `last_event_id`，服务端只推送增量

#### 对现有系统的影响

**影响范围：高**。这是前端架构的根基变更，影响 `app.js` 中几乎所有状态读取/写入路径。建议采用"并行实现 + 特性门控"策略：新版 StateManager 在 `state_v2.js` 中独立开发，旧版状态在过渡期内保留，通过编译开关或运行时 flag 切换。

**后端影响**：最小。主要影响 WS 帧：可能需要新增一个 `state_sync` 请求/响应帧类型。`run_bus_listener` 已经生产了完整的 `seq` 信息，前端只是没有利用它。

#### 方案对比

| 方案 | 复杂度 | 持久能力 | 迁移难度 | 推荐度 |
|------|--------|---------|---------|--------|
| A. localStorage 全量 | 低 | 受限 (~5MB) | 低 | ❌ 能力不足 |
| B. IndexedDB 全量 | 高 | 充足 | 高 | ✅ 正确方案 |
| C. 分级缓存 (推荐) | 中高 | 充足+高效 | 中 | ⭐ **推荐** |
| D. Service Worker 中转 → Cache API | 很高 | 足够 | 很高 | ❌ 过度设计 |

---

### 方向 B：前端路由框架（P0 — 与方向 A 同步进行）

#### 为什么需要

没有路由 = 没有深度链接 = 通知推送失去核心价值。如果推送通知点击后只能打开应用首页而非具体消息/房间/频道，那么推送功能的效果打 5 折。

此外，路由机制为未来功能开路：
- 聊天记录可以通过 URL 分享（`/rooms/abc123/threads/xyz`）
- 搜索结果可以引用具体消息（`/search?q=hello&msg_id=xyz`）
- 用户画像页面（`/users/uuid`）
- 管理工作区设置（`/workspace/uuid/settings`）

#### 核心挑战

- **hash vs history API 的选择**：hash 模式兼容性好（`#/rooms/abc`），但 URL 不美观；history API 需要服务端 fallback（所有路由指向 `index.html`）。考虑到当前是零路由，hash 模式起步更安全。
- **状态与路由的一致性**：当用户导航到 `/rooms/xyz` 时，路由必须与 `state.currentRoomId` 同步，反之亦然。这需要一个"单一事实源"的原则：URL 是权威，状态从 URL 推导。
- **现有代码中 `switchRoom()` 的修改**：目前 `switchRoom()` 只更新 DOM 和内部状态，需要改造为感知路由的跳转。

#### 预期的架构变更

```
路由层 (router.js)
  ├── 路由表: path → handler mapping
  ├── 导航函数: navigate(path, options)
  ├── URL 同步: hashchange / popstate → dispatch
  └── 深度链接解析: parseDeepLink(url) → {room, msg, thread}

路由表示例:
  / → 首页/房间列表
  /rooms/:roomId → 切换到指定房间
  /rooms/:roomId/threads/:threadId → 切换到线程
  /users/:userId → 查看用户
  /search?q=... → 搜索结果
```

- 所有 WebSocket 推送的 `onNotification` 回调生成深度链接，而非只改 title
- `switchRoom()` 改造为 `router.navigate()` 调用，内部调用现在 `switchRoom()` 的逻辑
- `api.js` 的 token 存储作为路由守卫的依赖（未登录→重定向到登录页）

#### 对现有系统的影响

**影响范围：中高**。需要：
1. 新增 `router.js`（约 200-300 行，纯前端路由）
2. 修改 `app.js` 中的 `switchRoom()` / `switchView()` 等导航调用
3. 修改 `ws.js` 中的通知处理以生成深度链接
4. 修改 `index.html` 中所有 `<a>` 和按钮的 `onclick` 处理——可能需要逐步替换

**后端影响**：无。路由完全是前端关注点。

**关键决策**：是否允许不刷新页面的"软导航"（hash 变更或 history.pushState）？答案是**必须**——这是 SPA 的核心优势。路由器应该在不触发完整页面加载的情况下切换状态。

---

### 方向 C：Hub 扇出架构优化（P1 — 下一阶段）

#### 为什么需要

当前 Hub 扇出的串行模式在处理大型房间（1000+ 成员 × 多设备）时会产生可感知的延迟。虽然 bounded channel + `try_send` 的设计让瓶颈远小于最坏估算，但架构上有几个不可持续的设计：

1. **`get_mut(pid)` 写锁串行化**——同一用户的多设备（手机、桌面、平板）在扇出时必须串行获取该 pid 的 DashMap shard 写锁
2. **扇出逻辑与业务逻辑同线程**——`fan_out_arc_inner` 在消息循环中直接执行，阻塞了消息消费的下一个事件处理
3. **`get_mut` 与 `register`/`unregister` 竞争**——高频扇出的写锁可能与用户重新连接/断开的写锁发生冲突

从业务价值看：1000+ 人房间的消息延迟在亚秒级是可接受的，但 5000+ 人房间需要更优的设计。这是"规模升级"的架构准备。

#### 核心挑战

- **扇出与消息传递时序**：如果并行化扇出，必须保证一个用户收到的消息顺序与其他用户一致（不能出现消息 2 比消息 1 先到达某些用户）。这要求 per-subscriber 的有序投递。
- **反向压力策略**：当前 `try_send` 的"丢则丢"策略在有界队列满时静默丢弃——这是可接受的（WebRTC 流允许丢帧），但应该有日志和 metric。
- **全局 vs 局部并行**：扇出延迟的瓶颈在"每个 pid 一把锁"，不是"所有 pid 一把锁"。优化策略应该是：

#### 预期的架构变更

**方案 A：per-pid 的专用队列（推荐）**
```
当前: Hub::fan_out_arc_inner
  → for pid in recipients: get_mut(pid) → try_send

优化后: 
Hub::fan_out_arc_inner
  → for pid in recipients: 
      let entry = connections.entry(pid)  // 获取或创建 channel
      entry.try_send(msg)  // 写入 per-pid 队列
      
  Worker 线程池:
    → 每个 worker 从多个 per-pid 队列批量消费
    → 对同一 pid 的消息保证顺序
    → 每批处理后 sleep(0) 让出线程
```

**方案 B：micro-batch + rayon（备选）**
```
1. 收集所有 (pid, msg) 对的向量
2. 使用 rayon 并行处理：chunk → get_mut + try_send
3. 等待所有 chunk 完成
```

**方案 C：维持现状 + Bound 监测**
```
1. 添加 FAN_OUT_DURATION 直方图 metric
2. 在 fan_out 耗时 > 50ms 时告警
3. 只在指标恶化时触发重构
```

**推荐方案 A**——per-pid 队列分离了"扇出调度"和"锁竞争"，且在架构上比 rayon 更优雅（避免引入 rayon 依赖）。方案 C 是"不决策的决策"。

#### 对现有系统的影响

**影响范围：中**。主要在 `ws/ws_impl/hub.rs` 内部：
- 重构 `Hub` 结构体，添加 per-pid 队列映射
- 引入 worker 线程/任务
- `register`/`unregister` 需要与 worker 协调（cancel per-pid 队列）
- 其他模块的接口不变（`Hub::fan_out_raw` 签名不变）

**度量先行**：在实际优化前，先添加 `FAN_OUT_DURATION` 直方图。如果 P99 延迟在 200ms 以内且服务器负载在目标范围内，优先级可降级。

---

### 方向 D：输入校验与错误处理架构层（P1 — 与 A/B 并行）

#### 为什么需要

这不是"加几个 `maxlength` 属性"的问题——是建立一个**前端自身的防御层**。当前前端的错误处理模式是"编程时顺手写的"而非"架构设计好的"，表现为：

- `.catch(() => {})` 散落在 5 个位置
- WS 错误无声处理
- 无前端输入校验
- 无操作确认/超时机制

从架构角度看，这些是**单一职责违反**：错误处理没有集中在一个抽象层，而是分散在业务逻辑中。

#### 核心挑战

- **错误分类和策略**：需要在架构层面区分"可恢复错误"（网络中断→等待重连）和"不可恢复错误"（用户权限被撤销→显示错误并阻止操作）
- **有界输入校验的维护成本**：一旦在前后端都有校验规则，每次更新规则需要同步修改——这是"写在两处的业务逻辑"的反模式。
- **离线操作的复杂性**：如果支持"先显示后确认"，就需要一个操作队列 + 确认超时 + 失败回滚机制。

#### 预期的架构变更

```
错误处理层 (error.js):
  ├── classifyError(err) → {type, recoverable, userMessage}
  ├── notifyUser(message, level) → toast / snackbar
  ├── handleWSError(err) → reconnect / fail / notify
  └── handleAPIError(err) → retry / redirect / notify
  
输入校验层 (validation.js):
  ├── validateMessage(text, blocks) → {valid, errors[]}
  ├── validateRoomName(name) → {valid, reason}
  ├── shared规则定义 (与后端的校验同步)
  └── formValidator(element, rules) → 绑定到 input/textarea
  
操作确认层 (delivery.js):
  ├── pendingOperation(id, timeout) → 注册待确认操作
  ├── confirmOperation(id) → 完成确认、移除 pending
  ├── timeoutCallback(id) → 通知用户、提供重试选项
  └── batchConfirm(ids) → 批量确认
```

- 将 `validation.js` 中的规则定义与后端共享（通过一个 JSON 格式导出，或直接在 JS 中维护权威副本）
- 引入 `DeliveryConfirmer` 类，替代当前 `pendingByTempId` 的无超时版本
- 建立"系统 toast"的统一通知渠道（`window.aeroNotify` 或 DOM 事件），替代 `console.warn`

#### 对现有系统的影响

**影响范围：中**。主要在新增架构层，对现有代码的侵入适中：
- `app.js`: 将 `send()` 调用包装在 `DeliveryConfirmer` 中
- 5 处 `.catch(() => {})` 替换为 `error.js` 的 `handleAPIError`
- 所有表单绑定 `validation.js` 的校验
- 不需要后端变更

---

### 方向 E：实时媒体管道统一架构（P2 — 远期）

#### 为什么需要

当前实时媒体（直播/通话）的架构呈现"功能岛"模式：
- WHIP 摄入 → str0m SDP → RTP 解包 → HLS writer
- 通话 SFU → 选择性转发 → RTCP
- SRT 摄入 → HSv5 → AES-CTR → MPEG-TS 分段
- call-bridge → 跨节点 UDP relay

这些能力各自独立实现，没有共享的**媒体管道抽象层**。导致：
- 针对新摄入类型（如 WebTransport）需要从头实现 SDP 协商→RTP 解包→分段管线
- 无法混合摄入和输出（如 RTMP 推流 + WHIP 拉流混合同一房间）
- 媒体度量和调试工具需要各自实现
- 故障模式多样化：每种媒体路径有自己的错误处理逻辑

从业务价值看：一个统一的媒体管道抽象层可以为未来的"统一媒体房间"场景做准备——任何参与者可以用任何协议接入（WHIP 浏览器推、RTMP OBS 推、SRT 专业摄录设备、WHEP 低延迟拉、HLS 大规模拉）。

#### 核心挑战

- **不同协议的语义差异**：WebRTC 的 SDP 协商 + ICE 是全双工的，RTMP 是单向推流，SRT 是基于 UDP 的可靠传输——它们之间的抽象层次不同。强行统一可能反而增加复杂度。
- **延迟要求不同**：通话要求 <300ms 端到端延迟，HLS 可以接受 5-30 秒。统一管道需要在早期就分离实时路径和缓存路径。
- **性能开销**：在 Rust 中增加抽象层意味着额外的 trait 分发开销。对于每帧处理（按 30fps 的 ~30ms 间隔），每次 trait 边界调用的成本都可能影响延迟。

#### 预期的架构变更

```
媒体管道抽象层 (MediaPipeline trait)
  │
  ├── MediaInput (trait): 推送方向
  │   ├── RtmpInput → RtmpIngest
  │   ├── WhipInput → WhipHandler
  │   ├── SrtInput → SrtIngest
  │   └── WebTransportInput (future)
  │
  ├── MediaProcessor (trait): 处理方向
  │   ├── H264Depacketizer (RTP→NAL)
  │   ├── H264Packetizer (NAL→RTP)
  │   ├── AudioOpusCodec
  │   ├── Transmuxer (RTP→MPEG-TS)
  │   └── Transrater (future)
  │
  └── MediaOutput (trait): 拉方向
      ├── HlsOutput → HlsWriter
      ├── WhepOutput → WHEP 应答器
      ├── SfuOutput → SfuForwarder
      └── CallEgressOutput → 跨节点桥
```

核心接口设计：
```rust
trait MediaInput: Send {
    fn id(&self) -> StreamId;
    fn poll_frame(&mut self, cx: &mut Context) -> Poll<Result<MediaFrame>>;
    fn stats(&self) -> MediaStats;
}

trait MediaOutput: Send {
    fn id(&self) -> StreamId;
    fn push_frame(&mut self, frame: MediaFrame) -> Result<()>;
    fn stats(&self) -> MediaStats;
}

trait MediaProcessor: Send {
    fn process(&mut self, input: MediaFrame) -> Result<Vec<MediaFrame>>;
}
```

#### 对现有系统的影响

**影响范围：非常大**。这是跨 crates 的重构，会影响 `aero-live-whip`、`aero-live-webrtc`、`aero-live-rtmp`、`aero-live-srt` 等多个 crate。建议：

1. **不要一次性重构**：从 `aero-live-whip` 开始，将其内部实现提取为 `MediaInput + MediaOutput` 的 trait 实现
2. **旧的接口保留为过期的**：在过渡期间，旧的 ingestion 接口仍然可用，但标记为 `#[deprecated]`
3. **度量先行**：在引入抽象层前，先在所有媒体路径上添加一致的 `MediaStats` 采集
4. **增量迁移**：每次重构一个 crate，验证性能和正确性一致后再推进下一个

---

## 3. 接口设计建议

### 3.1 前端核心接口原则

**原则 1：单一状态入口（Single State Gate）**
```js
// 不好的模式（当前）：
this.state.me = user;  // app.js 直接赋值
state.me = updated;     // 其他模块直接修改

// 好的模式：
stateManager.set('me', user);
stateManager.get('me');
stateManager.onChange('me', handler);
```
所有状态写操作通过 `StateManager`，它负责：
- 内存缓存更新
- 持久化到适当的存储层
- 触发订阅者的变更事件
- 日志和调试

**原则 2：错误不属于业务逻辑**
```js
// 不好的模式（当前）：
fetch(...).catch(() => {})  // 业务逻辑和错误处理混合

// 好的模式：
safeFetch(url, options).then(handleData);  // 错误由 `safeFetch` 层处理
safeSend(ws, message).then(onSent, onFailed);
```
错误处理是基础设施，不属于业务逻辑。每个"外部操作"（WebSocket 发送、API 调用）都经过一个包装层，该层统一处理错误分类、用户通知、重试策略。

**原则 3：路由驱动视图（Route-Driven View）**
```js
// 不好的模式（当前）：
switchRoom(roomId);  // 直接改 DOM + state

// 好的模式：
router.navigate('/rooms/' + roomId);
// router 内部调用 switchRoom(roomId) + syncURL()
```
视图状态从 URL 推导，而非独立维护。如果 URL 和内部状态不一致，以 URL 为准。

### 3.2 后端接口设计原则

**原则 1：扇出接口保持稳定**

`Hub::fan_out_raw` 的签名在 Hub 优化（方向 C）中不应改变。这是关键的外部契约：

```rust
// 当前: pub fn fan_out_raw(&self, event: Arc<RawEvent>, recipients: &[ParticipantId])
// 优化后: 不变
```

内部实现从串行变为 per-pid 队列时，外部模块不应感知到变化。

**原则 2：媒体管道接口谨慎抽象**

方向 E 的媒体管道抽象应该采用"装饰器模式"（Decorator），而非"替代者模式"（Replacer）。每个 `MediaInput` 实现包装现有的特化实现：

```rust
// 不删除旧的 RtmpInput
pub struct RtmpInput { ... }
impl RtmpInput { pub fn from_url(url: &str) -> Self; }

// 新的 trait 实现作为包装层
impl MediaInput for RtmpInputAdapter {
    fn poll_frame(&mut self, cx: &mut Context) -> Poll<Result<MediaFrame>> {
        // 委托给内部的 RtmpInput
        self.inner.poll_frame(cx)
    }
}
```

这样，旧的代码路径保持可用，新的抽象层可以逐步覆盖更多场景。

**原则 3：幂等性契约标准化**

当前 at-least-once 语义导致了多处幂等实现（ooo_bot 的 `ON CONFLICT DO NOTHING`、moderation_bot 的 soft-delete guard）。建议在 `aero-bus` 层引入一个**幂等键接口**：

```rust
// 在 aero-bus 中
pub trait IdempotentConsumer: EventConsumer {
    /// 事件的幂等键。如果存在，总线框架在投递前/后自动去重
    fn idempotency_key(event: &Self::Event) -> Option<String>;
    
    /// 是否消费该事件（幂等键命中时框架自动跳过）
    fn should_consume(event: &Self::Event, past_keys: &HashSet<String>) -> bool;
}
```

这是"框架层解决方案"而非"每个 bot 各搞各的"。

### 3.3 是否需要新抽象层

**需要**：

| 抽象层 | 位置 | 优先级 | 理由 |
|--------|------|--------|------|
| `StateManager` | web/state.js | P0 | 前端状态持久化的唯一可行方案 |
| `ErrorHandler` | web/error.js | P1 | 集中化错误处理策略 |
| `DeliveryConfirmer` | web/delivery.js | P1 | 替代当前无超时的 pending 机制 |
| `IdempotentConsumer` | aero-bus/src/idempotent.rs | P2 | 标准化幂等契约 |

**不需要**（在当前阶段）：

- **全局 ORM 或状态管理库**（Redux/MobX/Vuex）：当前 Web SPA 规模较小，引入大型框架反而增加复杂度。一个 200-300 行的手写 `StateManager` 足以支撑到产品需要真正前端框架的那天。
- **GraphQL 层**：当前 REST + WS 的接口组合足以满足需求。GraphQL 会增加后端复杂度（需要 GraphQL schema 与所有 Rust 类型对齐），在团队规模较小时不值得。
- **独立媒体网关服务**：当前媒体管道与主服务器在同一进程中运行。在流量增长到需要独立部署媒体节点之前，保持单体更简单。

### 3.4 向后兼容性策略

1. **分阶段迁移**：新旧接口并行存在至少一个发布周期
2. **Feature flags**：新接口在 `#[cfg(feature = "v2")]` 或运行时环境变量下启用
3. **Deprecation 周期**：旧接口标记为 `#[deprecated]`，两个版本后移除
4. **前端平行实现**：`state_v2.js` 与现有 `state.js` 分离，通过 `index.html` 的 script 标签选择

---

## 4. 技术选型

### 4.1 是否需要新框架/库

**方向 A（前端状态持久化）**：

| 选项 | 优点 | 缺点 | 推荐 |
|------|------|------|------|
| 纯 IndexedDB API | 零依赖 | API 晦涩、错误处理复杂 | ❌ |
| Dexie.js | 简化 IndexedDB、Promise 化 | 37KB gzip | ✅ **推荐** |
| idb-keyval | 极简（key-value） | 只适合简单存储 | ⚠️ 可配合 StateManager |
| lovefield (Google) | SQL-like 查询 | 停产 | ❌ |

**推荐**：Dexie.js 作为 IndexedDB 的封装层。它提供了 `db.transaction('rw', store, () => ...)` 的事务语义，可以保证状态写入的原子性——这是 localStorage 做不到的。它的 live query 在后续需要反应式 UI 时也很实用。

**方向 B（前端路由）**：

**不引入库**。前端路由在零路由的 SPA 中可以用 200-300 行原生 JS 实现（hash-based）。不需要 React Router 或 Vue Router。用一个 `Router` 类 + hashchange/popstate 监听即可满足当前需求。如果未来迁移到全栈前端框架（如 SvelteKit 或 Next.js），路由框架自然引入。

**方向 C（Hub 扇出并行化）**：

| 选项 | 优点 | 缺点 |
|------|------|------|
| rayon 线程池 | 简单易用 | 引入跨 crate 依赖；扇出不是 CPU 密集型 |
| tokio::spawn 任务 | 零新依赖 | 扇出是纯同步操作，不需要异步 |
| 手写 per-pid 队列 | 零依赖、可控 | 实现复杂度 |

**推荐**：方案 C（手写 per-pid 队列）。扇出是纯内存操作（锁+channel），不需要线程池或异步。一个 `HashMap<Pid, mpsc::Sender<Arc<Event>>>` + worker task 消耗这些队列即可。

**方向 D（输入校验/错误处理）**：

| 选项 | 优点 | 缺点 |
|------|------|------|
| 手写 | 零依赖、完全控制 | 需要自己维护 |
| validator.js | 功能全面 | 不够灵活、包体积大 |
| zod (zod on the web?) | TypeScript 友好 | JS 部署场景不适合 |

**推荐**：手写。前端的校验规则不多（消息长度、房间名、频道名、投票选项数），50 行代码覆盖所有场景。不需要第三方校验库。

### 4.2 第三方依赖的评估标准

当前项目 Rust 侧依赖管理良好（无 str0m 在 root 的重复声明，迁移编译期嵌入）。对于新引入的依赖，建议使用以下评估标准：

1. **候选必须满足**：
   - Rust: MSRV ≥ 1.80 (项目当前 MSRV)
   - JS: 无 Node.js 构建依赖（当前 web 是零工具链 ES2020）
   - 许可证兼容（MIT/Apache 2.0/BSD）
   - 最近 12 个月内有更新

2. **重量级依赖（需充分论证）**：
   - 增加 50%+ 以上编译时间（Rust）
   - 引入新的运行时代理或进程（JS 的 Redux/MobX）
   - 迫使项目迁移到特定的架构模式

3. **当前项目可以引入的低风险依赖**：
   - **Dexie.js**：IndexedDB 封装，纯浏览器端，无构建工具要求
   - **hls.js**：已在 `index.html` 中通过 CDN 引入
   - Rust `criterion`：性能基准测试工具（仅在 `[dev-dependencies]`）

4. **当前项目不应引入**：
   - **任何前端框架**（React/Vue/Svelte）：会迫使整个 web 架构重写，且当前规模不需要
   - **任何 Node.js 构建工具**（webpack/vite）：web 是零工具链的 ES2020，这是有意设计
   - **rayon**（仅为了 Hub 扇出）：过度，有更轻量的方案
   - **Redis 的锁框架**（redlock/redis-rs 锁）：当前 Redis 操作已经是原子性 sorted-set 操作，不需要分布式锁
   - **全链路 tracing 框架如 OpenTelemetry**：当前 metrics 通过 Prometheus 采集，OTLP 会增加复杂度

### 4.3 自建 vs 采购的决策

| 需求 | 自建 | 采购/开源集成 | 决策 |
|------|------|-------------|------|
| 前端状态管理 | 手写 StateManager (~200 行) | Redux/MobX/Zustand | ✅ **自建**——规模小、需求具体 |
| IndexedDB 封装 | 手写 | Dexie.js | ✅ **Dexie.js**——它的事务/索引功能手写成本高 |
| 前端路由 | 手写 Router (~200 行) | React Router/Vue Router | ✅ **自建**——不需要框架、不需要编译工具 |
| 输入校验 | 手写 (~50 行) | validator.js | ✅ **自建**——规则简单 |
| 媒体管道抽象 | 设计 trait 接口 | 无现成库 | ✅ **自建**——领域特定、现有 Rust 生态无对应物 |
| 幂等键框架 | 在 aero-bus 中实现 | 无现成库 | ✅ **自建**——依赖已有事件总线架构 |

**判断标准**：
- **自建**：依赖领域特定逻辑、与已有架构耦合紧密、简单到无需库
- **开源集成**：基础能力（浏览器存储 API 封装、CDN 托管的播放器）、已有成熟方案

---

## 5. 实施路线图

### 5.1 优先级排布

基于交叉验证报告的事实修正和架构评估，我给出的优先级排序：

```
P0 (立即开始 — 3-4 周)
├── 方向 B: 前端路由 (1-2 周) ← 最核心依赖
│   ├── Week 1: Router 基础框架 + hash 路由
│   └── Week 2: switchRoom/switchView 路由化
│
├── 方向 A: 前端状态持久化 (2-3 周)
│   ├── Week 1: StateManager 架构设计 + Dexie.js 集成
│   └── Week 2-3: 分级缓存实现 + WS 恢复逻辑
│
└── 注意事项：
    - 方向 A 和 B 有依赖关系：路由需要状态持久化来支持刷新后恢复
    - 建议并行启动，但 A 中的路由感知需要依赖 B 的接口

P1 (下一阶段 — 2-4 周)
├── 方向 D: 输入校验与错误处理架构层 (1-2 周)
│   ├── error.js 集中化
│   ├── validation.js 实现
│   └── DelieveryConfirmer 替代 pendingByTempId
│
├── 方向 C: Hub 扇出架构优化 (1-2 周)
│   ├── FAN_OUT_DURATION 直方图 metric 先行
│   ├── 仅当 P99 > 200ms 时实施 per-pid 队列优化
│   └── 当前优先级可低于 D，取决于实际负载
│
└── 注意事项：
    - D 与 A/B 有重叠：错误处理层修正方向 A 中发现的 .catch 问题
    - C 可独立并行，不影响其他方向

P2 (远期 — 4-8 周)
├── 方向 E: 媒体管道统一架构
│   ├── 从 aero-live-whip 开始原型
│   ├── 度量先行（统一 MediaStats）
│   └── 增量迁移
│
├── 幂等键框架 (aero-bus 扩展)
│   └── IdempotentConsumer trait + 后端幂等表
│
└── 通话/直播前端能力补齐
    └── WHIP 浏览器推流、WHEP 低延迟拉流、连麦
```

### 5.2 阶段划分和里程碑

#### 阶段 1：「产品化基础」（P0, 4-6 周）

**目标**：从"可用的原型"变为"可发布的 beta 产品"

**里程碑 1.1 — 路由可用**（Week 1-2）
- `Router` 类完成，支持 hash-based 路由
- URL ↔ 视图状态双向同步
- 所有导航调用通过 `router.navigate()` 
- 推送通知点击 → 深度链接跳转

**里程碑 1.2 — 页面刷新不丢失状态**（Week 3-5）
- `StateManager` 完成（内存 LRU + localStorage 热状态 + IndexedDB 冷状态）
- 页面加载后自动恢复最近房间列表和未读计数
- WebSocket 重连后增量同步
- pending 消息在刷新后仍然显示在 UI 上

**里程碑 1.3 — 完整体验闭环验证**（Week 5-6）
- 打开应用 → 恢复上次房间 → 发送消息 → 关闭标签页
- 打开新标签页 → 恢复相同房间和消息
- 收到推送通知 → 点击 → 跳转到正确消息上下文
- 刷新后 pending 消息显示正确状态

**验收标准**：
- 5 种用户旅程（打开、发送、通知、刷新、多标签页）无状态丢失
- 路由覆盖所有视图（房间、线程、搜索、用户页面）
- 手动测试通过，无需自动化测试（阶段 1 不要求端到端测试）

#### 阶段 2：「弹性与质量」（P1, 4-6 周）

**目标**：让用户即使在网络不稳定的情况下也能可靠使用

**里程碑 2.1 — 用户可见的错误处理**（Week 1-2）
- 所有 `.catch(() => {})` 替换为 `handleAPIError`，显示 toast 通知
- WS 断连：显示连接状态指示器，自动重连
- 发送失败：显示重试选项，保留消息草稿

**里程碑 2.2 — 输入校验完整化**（Week 2-3）
- `validation.js` 完成，覆盖所有表单（消息/房间名/投票/频道）
- 前端校验规则与后端同步（规则定义在 JSON 文件或 JS 常量中，单源）

**里程碑 2.3 — 扇出性能度量与优化**（Week 3-4）
- `FAN_OUT_DURATION` 直方图实现
- 根据实际数据决定是否实施 per-pid 队列优化
- 性能基准建立（基于实际的房间大小分布）

**里程碑 2.4 — 集成测试框架搭建**（Week 4-6）
- 引入简单的端到端测试（Playwright 或 Cypress）
- 覆盖 3 个关键用户旅程

**验收标准**：
- `make smoke-test` 覆盖前后端
- P99 扇出延迟 < 200ms（在 5000 人房间模拟下）
- 断线重连后消息不会丢失或重复
- 所有已知 `.catch(() => {})` 消除

#### 阶段 3：「媒体统一」（P2, 8-12 周）

**目标**：统一媒体管道、补齐通话/直播前端能力

**里程碑 3.1 — 媒体管道抽象原型**（Week 1-4）
- `MediaPipeline` trait 在 `aero-live-core` 中定义
- `aero-live-whip` 的 WHIP 摄入实现 `MediaInput`
- `HlsOutput` 实现 `MediaOutput`
- `MediaStats` 统一采集并链接到 Prometheus

**里程碑 3.2 — 浏览器推流能力**（Week 4-8）
- Web 端的 WHIP 推流实现（使用 `RTCPeerConnection` + `getUserMedia`）
- 与 `aero-live-whip` 的 WHIP 端点对接
- 浏览器端的"一键开播"流程

**里程碑 3.3 — SFU 与 CallEgress 生产接线**（Week 6-10）
- `SfuMediaSession::run` 在非测试代码中实例化
- `CallBridge` 的 `ensure_egress` 在实际媒体流中触发
- 两节点路由测试（同一主播、不同观众节点）

**里程碑 3.4 — 幂等键框架**（Week 8-12）
- `IdempotentConsumer` trait 在 `aero-bus` 中定义
- 现有 bot 迁移到新框架
- 幂等表在 Redis/Postgres 中的实现选择

**验收标准**：
- 浏览器 WHIP 推流 + HLS 拉流在演示场景可用
- 两节点通话桥接在 localhost 联调通过
- 至少 3 个 bot 使用了幂等键框架

### 5.3 风险点和缓解策略

| 风险 | 概率 | 影响 | 缓解策略 |
|------|------|------|---------|
| **方向 A 与 B 耦合导致依赖阻塞** | 中 | 高 | 先做路由（B），路由无需状态持久化即可独立工作。状态持久化（A）先做 localStorage 版（低复杂度的最小可行版本），再增量迁移到 IndexedDB |
| **状态持久化增加了前端复杂度** | 高 | 中 | 采用"优雅退化"策略：IndexedDB 不可用（如 Safari 隐私模式）时退回到 localStorage + 内存模式，不阻止用户使用 |
| **扇出优化过度干预** | 低 | 低 | 遵守"度量先行"原则：如果不维护度量数据，就不实施优化。实际案例中很多"性能问题"是猜测而非测量的 |
| **媒体管道抽象层的过度设计** | 中 | 中 | 限制抽象范围：只统一"单路流"场景（一个输入→一个输出），不试图覆盖"混合流""转码""转分辨率"等复杂场景。这些留到后续版本 |
| **前端架构层太多增加理解成本** | 高 | 中 | 每个架构层（Route/State/Error/Validation）的设计文档不超过 1 页，在新人 onboarding 时覆盖。代码注释标注每层的职责边界 |
| **幂等键框架引入数据库耦合** | 中 | 低 | 幂等键表在 Redis 中实现（SETNX + TTL），而非 Postgres。Redis 的 TTL 可以自动清理过期键，不需要 sweep 定时器 |
| **Rust 编译时间增加** | 低 | 低 | 新引入的 trait 和泛型在 `aero-bus` 中（已在更新路径上），不引入新 crate。媒体管道抽象在 `aero-live-core` 中，同样不新增 crate |
| **团队方向 A/B 实施周期过长** | 中 | 高 | 设定 6 周的硬截止日期。如果在 6 周内无法交付完整的分级缓存方案，先交付 localStorage-only 的最小可行版本。**不追求完美，追求可用** |

### 5.4 决策树

**状态持久化方案选择**：

```
是否接受页面刷新时状态丢失？
├── 是 → 维持当前架构（不可接受）
└── 否 → 是否需要离线工作能力？
    ├── 是 → IndexedDB + Dexie.js
    └── 否 → 是否需要持久化大消息体？
        ├── 是 → IndexedDB + 分层缓存
        └── 否 → localStorage 热状态 + API 冷数据的轻量方案
```

**Hub 扇出优化时机**：

```
是否有 FAN_OUT_DURATION 直方图数据？
├── 否 → 先添加 metric
└── 是 → P99 延迟 > 200ms?
    ├── 是 → 实施 per-pid 队列优化
    └── 否 → 维持现状，继续监控
```

**媒体管道抽象时机**：

```
是否要添加新的媒体通道类型（WebTransport/RTSP/...）？
├── 是 → 先做抽象（降低后续集成成本）
└── 否 → 已经在维护 3+ 种摄入类型？
    ├── 是 → 开始抽象提取（控制技术债）
    └── 否 → 推迟到下一个媒体相关功能
```

---

## 总结与核心建议

Aero IM 是一个前端投入与后端质量不平衡的系统。后端的事件驱动架构、crate 分层、常驻智能体体系是高质量的架构决策。而 Web SPA 处于"开发演示工具"的水平：状态全在内存、无路由、无错误处理策略、无输入校验——这不是开发者的个人能力问题，是架构投资的阶段性偏差。

### 优先级核心建议

1. **立即投资前端架构（P0）**：如果只能做一件事，就做"页面刷新后用户回到原来位置"。这直接影响产品是否可用。路由（方向 B）是基础，状态持久化（方向 A）是结果。两者顺序不可逆。

2. **下一个季度投资弹性（P1）**：错误处理、输入校验、扇出优化、幂等性框架。这些是"从可用到可靠"的跨越。

3. **远期投资媒体统一（P2）**：统一媒体管道抽象。这是"从可靠到规模化"的准备——在当前阶段不是瓶颈。

### 不要做

- ❌ **不要引入前端框架**。业务逻辑（发送消息/切换房间/直播互动）可以用原生 ES2020 表达。前端框架是"当状态管理和 DOM 更新的复杂度超过原生实现时"的选择，不是"为了用框架而用框架"的选择。
- ❌ **不要重写后端模块**。后端架构的问题不是新模块/新功能的设计问题，是已有模块的优化和演进问题。方向 C（Hub 优化）是改造而非重写，方向 E（媒体管道）是增量重构而非推倒重来。
- ❌ **不要解决不存在的问题**。方向 C 的优化只在"有数据证明需要优化"时才做。方向 E 的媒体抽象只在"需要添加新类型"或"维护成本不可接受"时才做。
