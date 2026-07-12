# 架构师分析报告：Aero IM — 消息、画布与审核系统的技术深度评估

> **分析范围**：输入文档中的四个方向（Message Block vs Canvas Schema、离线发送队列、Canvas 实时协作、审核通知闭环）以及三个补充观察。
> **视角**：架构师评估当前设计决策、识别技术债务、提出扩展方向，并给出可执行的路线图。

---

## 1. 架构评估

### 1.1 当前架构的战略优势

Aero IM 的事件驱动骨架（NATS → Hub → WS fan-out）是一个**经过深思熟虑的选择**，而且代码证据支持它的有效性：

| 优势 | 证据 | 架构价值 |
|------|------|----------|
| **进程内扇出 vs. 广播** | `Hub::fan_out_raw` bounded `mpsc` | 避免每房间独立 NATS subject 导致的 O(N) 订阅风暴；单进程内共享通道跨 50 个 WebSocket 连接只需一个 NATS consumer |
| **per-subject 单调 seq** | `bus/seq.rs`，发布前 mint | 客户端去重/排序不依赖时间戳；at-least-once 重投同一 seq 不会导致乱序 |
| **两套命名空间隔离** | `im.room.*` (durable) vs. `live.stream.*` (ephemeral) | 直播弹幕丢几条可接受，房间消息必须可靠。消费语义的差异直接在总线层编码，不是上层逻辑的 `if branch` |
| **集群级状态走 Redis** | presence / viewer / roster 全走 sorted-set | 避免单进程内存状态的水平扩问题。每实例心跳 `zadd` + `zremrangebyscore` 是成熟模式 |

### 1.2 架构断层：两个「设计哲学」未对齐

输入文档中提到的 Message/Canvas schema 差异，从架构角度看不是一个简单的「没对齐」，而是**两个完全不同的数据主权模型共存于同一系统，但没有被明确承认**。

```
Server-Owned 模型 (Message)              Client-Owned 模型 (Canvas)
┌─────────────────────────────────┐      ┌────────────────────────────────┐
│ Block = 强类型 enum             │      │ CanvasOp = serde_json::Value  │
│ Server drives:                  │      │ Server stores opaque JSONB    │
│  • AI pipeline (transcribe,     │      │ Client drives:                │
│    moderate, summarize, embed)  │      │  • CRDT merge                 │
│  • Search indexing (pgvector)   │      │  • Render state machine       │
│  • Rendering (render.js)        │      │  • Op validation              │
│  • Notification content extract │      │ Server only:                  │
│ Server validates:               │      │  • Persistence                │
│  • Block kind & shape           │      │  • Seq gap-free enforcement   │
│  • Content moderation           │      │  • ops_since polling          │
└─────────────────────────────────┘      └────────────────────────────────┘
```

**问题不在于两者不同，而在于没有文档化的 seam**。当未来功能需要跨越这个界限时（例如「AI 搜索 canvas 内容」或「canvas 变更触发通知」），开发人员不知道哪个模型是正确的基座。这就像是建筑蓝图里标注了「某面墙」但没有标注「承重墙 vs. 隔断墙」——结构师知道区别，但后续施工人员不知道。

**架构债务分类**：**未完成的抽象契约**。需要补的是一份设计文档（`docs/canvas-data-model.md`），不是代码。

### 1.3 最危险的架构债务：方向二（离线发送）

输入文档正确地识别了 `pendingByTempId` 的泄漏问题。我从系统层面补充：

```
用户点击发送
    ↓
optimisticAdd(blocks) ← 气泡立即显示
    ↓
ws.sendMessage() ← 不检查返回值（行 906）
    ↓
WebSocket send() ← 返回 false 时静默失败（ws.js:97）
    ↓
server 从未收到 → 无 ServerFrame::MessageSent
    ↓
pendingByTempId.delete() NEVER CALLED (仅 app.js:204 在确认时删除)
    ↓
⚠️ 用户看到永远旋转的气泡
```

这是一个**违背最少惊讶原则（POLA）** 的典型实现：乐观 UI 本应增强体验，但失败路径的缺失使其成为欺骗。它触及 IM 产品的核心诚信——用户对「消息已发送」的信任。

**从架构层面看，根本原因是**：`optimisticAdd`（UI 层）、`sendMessage`（协议层）、`send()`（传输层）三者之间没有失败回调的契约。WebSocket 是异步的，`send()` 的 boolean 返回值只表示「是否写入内核缓冲区」，不代表「是否被 server 接收」。架构上缺失的是：

- 一个**传输层的可靠性抽象**（类似 TCP 的 ACK，但基于应用层确认）
- 一个**重试/回退机制**（传输失败后自动重试，而不是静默丢弃）

### 1.4 审核系统的架构裂痕

`review_report` 不做通知不做审计，这不是简单的「忘了调用」，而是**职责粒度的架构问题**：

```rust
// message_reports.rs:216-296
async fn review_report(repo, report_id, reviewer_id, action, note) -> Result<()> {
    // 1. 验证权限 ✓
    // 2. 更新 report 状态 ✓  
    // 3. 软删消息（如果 remove=true）✓
    // MISSING: 4. 通知举报人审核结果
    // MISSING: 5. 写入 audit_events
    // MISSING: 6. 通知被举报人（如果申诉）
}
```

函数名为 `review_report`，但从业务语义来看，它应该返回的不只是 `Result`，而是一个**领域事件**：

```rust
// 架构上应该是什么样（非代码，是契约）
enum ReviewOutcome {
    Upheld { message_removed: bool },
    Dismissed,
    AlreadyReviewed { previous_outcome: ... },
}

// review_report 返回此事件 → 调用方负责通知
```

当前设计将这个 events-out 的责任向上推给了 PATCH handler，但 handler 也没处理。这是**职责下沉失败**——中层函数知道自己产生了什么业务效果，但没有把效果暴露出来。

---

## 2. 扩展方向

### 方向 A：可靠性层抽象（Reliable Delivery Layer）

**为什么需要**：
- 当前 `ws.send()` 的 boolean 返回值是 socket 状态指示，不是消息送达确认
- 方向二的问题（幽灵消息、不可靠成功幻觉）是系统性风险，不是单一函数问题
- 未来的上传文件、预览生成、AI 写作等异步操作都需要类似的可靠语义

**技术难点**：
- WebSocket 没有内置 ACK 机制——需要在应用层实现 request-response 模式
- 需要在 `pendingByTempId` 添加超时自动重试 → 上限次数后 fallback 到「消息暂存」
- 多 tab 场景下重试状态的共享（IndexedDB vs sessionStorage 的权衡）

**架构变更**：

```
当前：
ws.sendMessage(roomId, blocks) → ws.js:send() → WebSocket.send(socket, msg)

未来：
ws.sendMessage(roomId, blocks) → 
    new OutboxQueue.enqueue({ tempId, roomId, blocks }) → 
        ws.sendWithAck(msg, timeout=5s) → 
            onAck → OutboxQueue.dequeue(tempId)
            onTimeout → OutboxQueue.retry(tempId, maxRetries=3)
            onFinalFailure → OutboxQueue.fail(tempId) → UI 显示「发送失败」
```

**需要新增的模块**：

| 模块 | 职责 | 复杂度 |
|------|------|--------|
| `OutboxQueue` | 内存 + sessionStorage 双写，保证 crash-recovery 不丢 | 中等 |
| `WebSocketAck` | 请求 ID + 应用层 ACK（ServerFrame::Ack { request_id }） | 低 |
| `RetryPolicy` | 指数退避 + 最大重试次数 + 最终失败回调 | 低 |
| `PendingMessageStore` | sessionStorage 的 CRUD 封装 | 低 |

**对现有系统影响**：
- 前端新增约 200-300 行 JS
- Server 端需在 `ws.rs` 处理 `SendMessage` 后返回 `ServerFrame::MessageSent`（`msg_id + temp_id`）——**这是最小的 server 变更**
- 已有代码无需重写，只需插入 `OutboxQueue` 中间层

---

### 方向 B：Canvas 数据模型的正式契约

**为什么需要**：
- 当前「opaque JSONB」的 design debt 阻止了搜索、通知、AI 三大场景
- Canvas 是 IM 产品的差异化功能（Notion-like 协作 + 实时 IM 混合）
- 没有 schema 意味着没有 server 侧的合法性校验——恶意 client 可以写入任意 JSON

**技术难点**：
- Canvas blocks 类型的定义要与 Message `Block` 保持语义一致，但不能完全合并（因为 Canvas 有 `Table`/`Embed`/`Image` 等 Message 没有的类型）
- 需要设计一个灵活的 schema 系统，既能 server 校验，又能为未来的自定义 block 留空间
- CRDT merge 引擎的客户端实现（Y.js / Automerge 或自建）

**架构变更**：

```
Phase 1: Schema 定义（P2）
- common/src/canvas_blocks.rs 定义强类型 enum
- server 侧校验 CanvasOp 中的 block 结构
- render.js 添加 canvas block 的只读渲染

Phase 2: 搜索索引（P3）
- Canvas content 提取为 text → pgvector 嵌入
- 搜索结果中包含 canvas 片段

Phase 3: 实时协作（P4）
- canvas.{id} NATS subject
- 客户端 CRDT merge engine
- Hub 注册 + WS 帧定义
```

**与 Message `RichBlock` trait 的关系**：

```
trait RichBlock {
    fn as_rich_text(&self) -> Vec<RichSpan>;    // 文本提取（搜索/通知/AI）
    fn as_block_html(&self) -> String;          // 渲染（UI）
    fn to_search_text(&self) -> String;         // 向量搜索
}
```

Message `Block` 直接实现该 trait。Canvas block 通过从 JSONB 反序列化到 `CanvasBlock` enum 后也实现该 trait。**搜索索引管线不需要知道来源是 Message 还是 Canvas**。

---

### 方向 C：审核系统的领域事件总线

**为什么需要**：
- `review_report` 不做通知不做审计是 Trust & Safety 的合规缺口
- 需要支持自动化审核流程（AI 审查 + 人工复核 + 通知闭环）
- 企业客户的合规审计（SOC 2）需要 audit trail

**技术难点**：
- 审核员操作后需要通知举报人和被举报人——但被举报人只有在消息被删除时才需要通知（否则不应暴露审核动作）
- 需要避免审核决策被滥用（审核员删除的消息需要永久保留在 audit_events 中）

**架构变更**：

```
当前：
review_report → 更新 DB → 返回

未来：
review_report → 更新 DB →  emit ReviewEvent {
    report_id, 
    action: Upheld | Dismissed,
    notified_reporter: bool,
    notified_reported: bool,
    audit_entry_id: Uuid,
}
```

- `ReviewEvent` 不是 Nats event（不跨实例），而是**进程内 event bus**（tokio broadcast channel 或 Hub 扩展）
- `push_bot` 可从 `ReviewEvent` 获得 push payload（新增 subject `moderation.review.*`）
- 不需要新的 bus consumer，复用现有 `Notify` 架构

**对现有系统影响**：
- `async fn review_report` 改为返回 `ReviewOutcome`
- `PATCH /api/reports/:id` handler 接住 `ReviewOutcome` 后调用 `notif_prefs::notify_reporter` + `audit_events::log`
- 无需新增 NATS subject（至少第一阶段不需要）
- 约 50 行 Rust 业务逻辑 + 20 行通知调用

---

### 方向 D：WebSocket 协议层的框架化

**为什么需要**：
- 当前 `ws.js` 中不同消息类型的处理逻辑分散在 `ws.on('msg:xxx', ...)` 中，没有统一的框架
- 随着 Canvas 实时 ops、审核通知、AI 流式响应等新功能加入，帧类型会超过 30 种
- 目前没有请求-响应匹配机制（每个 `ClientFrame` 没有 `request_id` 字段，无法关联响应）

**技术难点**：
- 需要在不破坏现有帧结构的前提下引入 `request_id`（可选字段，向后兼容）
- 需要设计一个 middleware 链（如 `{ log, auth, rateLimit, handle }`）

**架构变更**：

```
// 当前
ws.addEventListener('message', e => {
    const frame = JSON.parse(e.data);
    if (frame.type === 'msg:message') { ... }
    if (frame.type === 'msg:reaction') { ... }
    // ... 15-20 个 if-else
});

// 未来
ws.addHandler('msg:message', { 
    ack: true,                      // 需要 ACK
    transform: parseBlocks,         // 预处理
    handle: renderMessage,          // 主处理
    onError: showNotification       // 错误处理
});
```

**这不是紧急项**，但应该作为**标准**在新功能开发时逐步采用，而不是一次性重构。新加的帧类型（如 `msg:canvas-op`）应使用新模式，老帧类型保留现样。

---

### 方向 E：消息渲染管线的统一抽象

**为什么需要**：
- 输入文档指出 Markdown 路径的 `optimisticAdd` 与 server 渲染结果不一致（plain text → rich text 闪烁）
- 当前 `renderMessage` 函数（约 400 行 `app.js`）直接把 `Block` 转 DOM 元素——没有中间表示层
- 未来的 Canvas block 只读渲染需要同样的 span 渲染能力

**技术难点**：
- 浏览器端 markdown->blocks 渲染需要实现一个 mini-parser（现有 server 端 `markdown.rs`）
- 不能要求 client 和 server 的 parser 完全一致——总会有细微分歧

**架构变更**：

```
抽象 render pipeline:

Block / CanvasBlock
    ↓
RichSpan[]   ← 中间表示（{ text, bold?, italic?, code?, link?, mention? }）
    ↓
DOM 节点      ← 一个 renderer 处理所有来源
```

这意味着：

1. 前端需要实现一个轻量级 `markdown_to_blocks` 函数（复用 `server/markdown.rs` 的逻辑子集）
2. `optimisticAdd` 在 Markdown 路径下使用这个函数生成 blocks，而不是 `[{ type: 'text', content: md }]`
3. 这样乐观渲染和 server 确认后的渲染是一致的——消除闪烁

---

## 3. 接口设计建议

### 3.1 关键接口设计原则

对于 Aero IM 的架构演进，我建议采纳以下原则。注意这些是**设计契约**，不是代码——输入文档中已经展示了一个需要明确契约的系统，我在这里把它形式化：

**原则一：所有「产生影响」的函数必须返回「发生了什么事」**
- 违反示例：`review_report` 返回 `Result<()>` 但实际产生了 `{ 删除消息, 更新状态, 可能在审核日志 }` 的效果
- 正确做法：返回一个描述副作用的 enum/struct，让调用方决定是否需要通知/审计/事件溯源

**原则二：错误必须落在用户可见的地方，不能静默吞噬**
- 违反示例：`ws.send()` 返回 `false`，调用方不理，用户看到「发送中」永远不消失
- 正确做法：任何 `send` 操作必须有一个 `onError` 回调，将错误状态反映到 UI

**原则三：server-owned 数据的形状必须在 server 校验**
- 违反示例：`CanvasOp::op` 是 `serde_json::Value`，server 不校验就存
- 正确做法：如果 server 存储它，server 就必须能解释它（至少能做结构校验）

**原则四：实时通道覆盖的数据必须走实时通道**
- 违反示例：`canvas ops` 不经过 `im.room.*`，导致协作变更不实时
- 正确做法：任何需要在 500ms 内同步到其他客户端的数据，必须注册到 Hub 或等效 NATS subject

### 3.2 是否需要新的抽象层

| 抽象层 | 需要 | 理由 |
|--------|------|------|
| 传输可靠性层（OutboxQueue） | **是** | 方向二的根本解，系统性解决「send() 不返回 ACK」的问题 |
| 领域事件（ReviewEvent） | **否（用现有 Notify 替代）** | 审核通知不需要新抽象，复用现有 `notif_prefs` + `push_bot` 即可。新抽象的成本（bus consumer + Hub 注册）在方向四场景下是 over-engineering |
| 渲染中间件（RichSpan） | **推荐** | 解决方向二的 Markdown 闪烁 + 为方向一的 Canvas 只读渲染铺路。但应该增量引入，不一次性 refactor 所有 render 路径 |
| WS 协议框架 | **否（采用指导原则替代）** | 现有 if-else 派发在 30 种帧类型以下可行。应该做的是**为新增帧类型制定模板**，而不是重构已有 |



### 3.3 向后兼容性策略

| 变更 | 兼容策略 |
|------|----------|
| 新增 `request_id` 字段 | 可选字段，老 client 不发送则 server 不返回 ACK——**不破坏任何现有代码** |
| `ReviewOutcome` 结构变更 | 新结构在 `Result` 内部，只影响调用方。API 响应体不暴露此结构——**无外部影响** |
| Canvas schema 强类型化 | 迁移期：读路径兼容旧 JSONB 和新 typed。写路径：双重写入（强类型校验 + 旧 JSONB 兜底） |
| `ServerFrame::MessageSent` 新增 `temp_id` 字段 | 新 client 用 `temp_id` 匹配，老 client 忽略。**完全向后兼容** |

---

## 4. 技术选型

### 4.1 各方向的技术栈评估

| 方向 | 建议方案 | 理由 | 替代方案 | 决策依据 |
|------|----------|------|----------|----------|
| 离线队列存储 | **sessionStorage + 内存** | `beforeunload` 可同步写；多 tab 问题当前可接受 | IndexedDB（更持久但异步）；Service Worker（overkill） | 场景临时性 + 实现复杂度最低 |
| 消息重试策略 | **指数退避 (1s, 2s, 4s, 8s) + 上限 3 次** | 避免网络瞬时故障误判；3 次后用户介入 | 无限重试（会堆积太旧的消息）；始终重试（WiFi 断开场景不合理） | 用户预期管理 + 资源消耗平衡 |
| Canvas CRDT | **起步不引入** | 当前 CRDT 引擎 200KB+ min+gzip（Y.js 131KB）；无客户端消费引擎；server 不做 merge | Automerge（更重）；自建 OT（复杂度高） | 产品阶段 + 带宽/加载时间考量 |
| Canvas schema 校验 | **serde DenyUnknownFields** | 利用现有 Rust 强类型系统，零新依赖 | JSON Schema 校验（需要引入库）；手写校验（易遗漏） | 已有 serde，零额外成本 |
| Markdown 前端渲染 | **markdown-to-blocks 轻量函数** | 约 50 行 JS，纯正则 + 状态机，不引入依赖 | marked.js（21KB min+gzip，但产出 HTML 而非 blocks）；prosemirror（重） | 只产符合 `Block` 的形状，不产 HTML |

### 4.2 自建 vs. 引入原则

**自建条件**（全部满足才自建）：
1. 核心逻辑 ≤200 行代码
2. 与现有类型系统深度耦合（如 `Block` enum 的前端等价物）
3. 不需要复杂算法（如 CRDT merge）

**引入条件**（全部满足才引入）：
1. 代码量 >500 行
2. 有活跃维护 + 安全审计
3. 可 tree-shaking 减少体积

**当前场景评估**：

| 模块 | 自建 | 引入 | 理由 |
|------|------|------|------|
| OutboxQueue | ✅ | — | ≤100 行 JS，核心是 Map + sessionStorage CRUD |
| retry policy | ✅ | — | ≤30 行 JS，纯逻辑 |
| CRDT engine | — | ❌ | 都不满足——无客户端消费引擎，引入无用 |
| markdown->blocks | ✅ | — | ≤80 行 JS，与现有 `Block` 类型一致 |

### 4.3 关于「方向二」技术方案的详细推演

输入文档问道：
> 你认为方向二的 OutboxQueue 应该用 IndexedDB 还是 sessionStorage？

我推荐的方案是**三层策略**：

```
Layer 1: 内存队列（Mutex<Vec<PendingMessage>>）
  └─ 正常操作：入队、按 ACK 出队、超时重试
  └─ 页面关闭前：同步写入 Layer 2

Layer 2: sessionStorage（crash-recovery 保底）
  └─ beforeunload 时同步写入
  └─ 页面加载时读取到 Layer 1
  └─ 同步 setItem 在 beforeunload 中可用

Layer 3: (未来) IndexedDB（跨 tab 共享）
  └─ 仅在确认需要多 tab 支持时加入
  └─ 通过 BroadcastChannel API 通知其他 tab
```

**理由**：

1. **sessionStorage 在 beforeunload 中可用**是标准行为（`setItem` 是同步的，不受页面关闭影响）
2. **多 tab 问题可以接受**——当前产品不支持同用户多 tab 发送消息（这会导致有序性问题），所以 sessionStorage 不共享反而是正确的语义
3. **IndexedDB 的异步问题**：`beforeunload` 中的异步操作（包括 `idb.put()`）是不保证完成的——浏览器可能在回调返回前就终止页面
4. 如果把 `beforeunload` 换成 `visibilitychange` + 页面变 hidden 时写数据库，IndexedDB 的写操作有机会在页面冻结前完成（约 200ms），但还是有竞态

**我推荐 sessionStorage 而非 IndexedDB**，因为其失败模型更可控。

---

## 5. 实施路线图

### 5.1 优先级排序（对齐输入文档，补充依赖分析）

```
P0 ──────────────────────────────────────────────
 方向二（离线发送队列） 
    依赖：无。纯前端。可独立落地。
    风险：低。不走 agent 导致不一致。
    影响：全局 UI 可靠性。

P1 ──────────────────────────────────────────────
 方向四（审核闭环）
    依赖：需确认 Notify 系统的消息模板是否支持「报告审核结果」
    风险：低。纯后端逻辑补齐。
    影响：Trust & Safety 完整闭环。
 
 └─ 输入文档补充建议：修复方向四中 404 状态码问题
     （双击按钮时返回 200 { status: "already_reviewed" }）
 
 └─ 输入文档补充建议：review_report 中补充 audit_events 写入

P2 ──────────────────────────────────────────────
 方向一（Schema 统一：RichBlock trait）
    依赖：需要 Canvas block 类型定义的 RFC 文档
    风险：中等。统一的 trait 设计要兼顾搜索、渲染、通知三大消费方
    影响：为方向三铺路；改善搜索和通知质量。
 
 └─ 输入文档补充建议：同时修复 Markdown 前端渲染问题
     （optimisticAdd 在 Markdown 路径使用前端 markdown->blocks）

P3 ──────────────────────────────────────────────
 └─ 方向一第二阶段：Canvas schema 强类型化
    依赖：需要 migration 将 JSONB 转为强类型列
    风险：中等。需要处理存量数据兼容

P4 ──────────────────────────────────────────────
 方向三（Canvas 实时协作）
    依赖：方向一 + 客户端 CRDT 引擎
    风险：高。新 NATS subject + 新 Hub 注册 + 新 WS 帧 + 新 JS 模块
    前置于方向一落地后

 └─ 补充：Web SPA 零 Canvas UI 的填补——至少需要一个只读预览
     （render.js 处理 ServerFrame::CanvasUpdate）
```

### 5.2 阶段划分与里程碑

#### 阶段 1：可靠性修复（P0，1-2 天）

| 里程碑 | 交付物 | 验收标准 |
|--------|--------|----------|
| M1.1 | `OutboxQueue` 实现 + `sessionStorage` 持久化 | 关闭标签页后重开，未确认消息仍显示为 pending |
| M1.2 | `WebSocket::send` 返回值检查 + 重试逻辑 | 模拟断网后发送，消息不丢 + 恢复网络后自动重试 |
| M1.3 | 失败时的 UI 反馈（「发送失败，点击重试」） | 超过重试上限后消息气泡显示错误状态 + 可手动重发 |
| M1.4 | `server` 端为 `SendMessage`/`SendMarkdown` 响应添加 `temp_id` | `ServerFrame::MessageSent` 包含 `temp_id` 字段 |

**关键风险**：`temp_id` 的 server 端传输需要改 `ws_impl/frame.rs` 和 `ws_impl/handler.rs` 中的 `send_message` handler。这是前端修改的最小 server 依赖。

#### 阶段 2：审核闭环（P1，1 天）

| 里程碑 | 交付物 | 验收标准 |
|--------|--------|----------|
| M2.1 | `review_report` 返回 `ReviewOutcome` | 返回值包含 `{ action, report_id, auditor_id, timestamp }` |
| M2.2 | PATCH handler 调用 `notif_prefs::notify_reporter` | 举报人在 inbox 中看到「你的举报已被处理」通知 |
| M2.3 | `review_report` 写入 `audit_events` | audit trail 可追溯审核操作 |
| M2.4 | 404 → 200 状态码修复 | 重复点击审核按钮返回 `{ status: "already_reviewed" }` |

**风险**：需要确认 `Notify` 系统是否支持「审核结果」这一通知类型——如果 `NotifyKind` 中没有对应的 variant，需要加一个。这是最小变更。

#### 阶段 3：Schema 统一设计（P2，2-3 天设计 + 1 天实现核心 trait）

| 里程碑 | 交付物 | 验收标准 |
|--------|--------|----------|
| M3.1 | 设计文档 `docs/message-canvas-schema-unification.md` | 定义 `RichBlock` trait + `RichSpan` 结构 + Canvas blocks 类型树 |
| M3.2 | Rust 侧 `RichBlock` trait 实现（Message `Block`） | `Block` 的现有 render 路径不破坏，新 trait 作为扩展 |
| M3.3 | 前端 `markdown_to_blocks` 函数 | 乐观渲染和 server 确认渲染结果一致（块级别） |
| M3.4 | 前端 `render_message` 重构使用 `RichSpan` 中间表示 | 无功能性变化，只是 render 管线重构 |

**关键设计决策**：

- `RichBlock` trait 放在 `common/src/rich_block.rs`（与 `common/src/model/` 平级）
- `CanvasBlock` enum 结构参照 `Block` 但扩展 `Table`/`Embed`/`Image` 等类型
- 搜索索引管线通过 `to_search_text()` 统一消费 Message 和 Canvas

#### 阶段 4：Canvas 演进（P3-P4，取决于产品优先级）

本阶段不在当前路线图约束范围内，仅在方向一落地后作为自然延伸触发的可选阶段。

### 5.3 风险矩阵

| 风险 | 概率 | 影响 | 等级 | 缓解策略 |
|------|------|------|------|----------|
| sessionStorage 在移动端浏览器中被清除 | 中 | 低 | **M** | 纯 UX 问题——失去的只是 crash-recovery 能力，不是最终发送能力。重连后用户会看到错误状态 |
| `temp_id` 字段被旧 server 版本忽略 | 高（部署过渡期） | 低 | **M** | 新前端需要能处理旧 server 不返回 `temp_id` 的情况——此时退化为当前行为（无 ACK） |
| 审核通知误发给已删除用户 | 中 | 低 | **L** | `notif_prefs::notify` 在发送前会检查 participant 状态 |
| Canvas schema 迁移时存量数据丢失 | 低 | 高 | **H** | 双读兼容 + 后台 backfill 任务（embedding_backfill 模式） |
| `RichBlock` trait 设计过于抽象，导致 implementor 负担重 | 中 | 中 | **M** | 默认为 `Block` 提供默认 trait 实现，新类型只需覆盖有差异的方法 |

### 5.4 与输入文档的主要分歧和共识

| 观点 | 输入文档 | 本分析 | 原因 |
|------|----------|--------|------|
| 优先级 | P0: 方向二 → P1: 方向四 → P2: 方向一 → P3: 方向三 | ✅ 一致 | — |
| 方向四中 400 状态码 | 补充观察中指出 | ✅ 纳入路线图 M2.4 | 是 UX 的稳健性改进 |
| Web SPA 零 Canvas UI 的严重性 | "比表面更深" | ✅ 补充了对 `render.js` 缺失的分析 | 但优先级仍为 P4——没有编辑器之前，只读预览的商业价值低 |
| Markdown 闪烁问题 | 补充 1 中指出 | ✅ 纳入路线图 M3.3（与 Schema 统一同步修复） | 但优先级低于 P0——这是体验瑕疵，不是功能缺失 |
| direction 四的 `ReviewEvent` 概念 | 未提及 | ❌ 不引入新抽象 | 现有 `Notify` 系统已足够覆盖 |
| direction 二的 ACK 机制 | 未明确指出 | ✅ 明确为 `temp_id` 匹配 | 是可靠性层的核心机制 |

---

## 6. 总结：架构师的核心判断

Aero IM 的架构基础（事件驱动 + 进程内扇出 + 集群级 Redis 状态）是**正确的选择**。四个方向的问题都不是「架构错了」，而是**「架构决策没有收口」**：

1. **方向一**：两个数据主权模型的 seam 没有归档——补文档而非补代码
2. **方向二**：乐观 UI 的失败路径没有设计——补可靠性层而非改架构
3. **方向三**：实时通道没有覆盖 canvas 数据——补 subject + Hub 注册，但依赖前端消费引擎
4. **方向四**：`review_report` 的副作用没有暴露为返回值——补 `ReviewOutcome` 而非改消息架构

所有这些问题的修复路径都在现有骨架内，不需要推翻重建。这是健康架构的标志——**发现断裂了，但可以通过补充契约而非重构来修复**。

最值得立即采取的行动是：
1. **今天开始**方向二的 OutboxQueue（P0，纯前端，无 server 依赖可先开始）
2. **今天开始**方向四的 review 通知（P1，纯后端，无前端依赖）
3. **本周内**输出 `docs/message-canvas-schema-unification.md` 设计文档（P2，为下 sprint 做准备）
