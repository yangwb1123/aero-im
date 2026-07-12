已经深入阅读了所有 4 个方向涉及的源码。现在呈现架构分析。

---

# 架构分析：Aero IM 客户端存在的 4 个缺口

## 1. 架构评估

### 当前架构的优势

**1a. 清晰的关注点分离。** 代码库严格遵循模块化——`api.js`（HTTP 传输）、`ws.js`（WebSocket 客户端）、`context.js`（共享单例与 DOM 引用）和 `app.js`（业务编排）。`context.js` 作为共享脊柱，允许域模块（`calls.js`、`media.js`、`search.js`）通过 ESM 单例语义导入相同的活状态，而无需打包工具或 DI 容器——这是零依赖 SPA 的巧妙设计。

**1b. 状态优先的架构。** 所有运行时状态集中在 `state` 对象中（`context.js:17-64`），这使得推理数据流相对容易，也使得未来支持像 `window.state` 导出这样的开发工具成为可能。`messagesByRoom`、`pendingByTempId` 等 map 结构保持了 O(1) 的查找复杂度。

**1c. 服务端具备完善的会话和推送协议。** 后端完整实现了 refresh token 生命周期（`session.rs`，含 `refresh_ttl_secs`）、backfill 协议（`?since=` 游标）、presence over Redis sorted-set——但客户端未能充分利用这些能力。

### 关键的架构债务

**1d. 请求函数缺少拦截器模式（方向 1 的根因）。** `api.js` 中的 `request()` 函数是纯无状态函数：接收 HTTP 参数，发送请求，返回响应。401 响应只能通过 `try/catch` 由每个调用点独立处理，而这又不可避免地落入 `app.js:612` 的 `if (err instanceof ApiError && err.status === 401) forceReauth()`。没有中间件/拦截器层来：
- 嗅探 401 并在返回错误之前尝试静默 refresh
- 追踪所有活跃请求并在 refresh 期间让它们排队
- 将 refresh 逻辑与业务逻辑解耦

这是一个架构层面的漏洞：**刷新 token 的能力存在，但无法被插入请求生命周期。** 修复方案需要引入一个请求拦截器，但这必须与现有的无状态设计兼容。

**1e. 乐观 UI 与服务器确认之间缺少承诺/契约（方向 2）。** `optimisticAdd` + `findPendingMatch` 模式假定服务器回显包含相同的 `blocks` 和 `created_at`，但这是一个脆弱的启发式方法：
- 非文本块（文件/语音）会在 `textOf()` 中被静默丢弃
- 如果服务端修改了 `blocks`（如 Markdown 解析增加了 spans、审核修改了内容），匹配就可能失败
- 15 秒窗口本质上是竞态条件

**核心设计问题**：pending 消息没有不可变的本地临时 ID，客户端可以用它来可靠地将服务器确认映射回 optimistic 条目。`tempId` 是唯一的，但仅被存储在 `state.pendingByTempId` 中——服务器永远不会看到它。修复合约需要发送 `tempId` 到服务器并让服务器将其回显，或实现有序交付确认。

**1f. DOM 渲染不是声明式的（方向 3）。** `rerenderCurrentRoom()` 通过 `replaceChildren()` 等方式直接操作 DOM，而不是使用基于 diff 的虚拟 DOM 方法。这使得：
- 在重建之间保持滚动位置变得困难
- 实现增量更新（动画、过渡）变得复杂
- 在非 React 设置中测试 UI 逻辑变得具有挑战性

虽然像 React 这样的框架在这里可能过于重量级，但引入像 `lit-html` 这样轻量级的基于模板的渲染器或使用 `replaceChildren()` 的简单有序键控渲染，将简化对滚动位置保留和新消息增量插入的支持。

**1g. 重连状态恢复是特设的，而不是系统的（方向 4，做对的部分）。** 客户端在 `ws.on('open')` 中恢复房间成员身份和直播流订阅，但状态恢复逻辑散布在代码中：一些状态在 `connect()` 时通过 `_lastSeen` / `_seqGate` 隐式恢复，一些通过 `replayChanges()` 显式恢复，而像 `watchedStreams` 这样的其他状态则通过特设循环恢复。没有单一的"将客户端恢复到此状态"的入口点。

### 决策合理性

| 决策 | 合理吗？ | 理由 |
|--------|---------|---------|
| 无打包工具的原生 ESM | ✅ 好 | 零依赖可降低复杂度，适合规模可控的 SPA |
| 通过 `[data-msg-id]` 避免虚拟 DOM | ⚠️ 可以接受 | 对于 300 行/房间以下适用，但方向 3 和方向 2 中需要更结构化的方法 |
| 通过 localStorage 的持久化身份验证 | ✅ 好 | 与服务器 token 模型匹配，在页面刷新后仍然存在 |
| 通过 `textOf` 的乐观匹配 | ❌ 有缺陷 | 服务端需要回显临时 ID，或客户端需要在确认后通过 REST 进行验证 |
| 连接流失时通过 `state` 重建 DOM | ⚠️ 有风险 | 没有 vdom，状态恢复和 DOM 渲染必须显式同步 |

---

## 2. 扩展方向

### 方向 A（P0）：请求管道拦截器 + 静默 token refresh

**为什么需要**：这是"存在性"缺陷。如果没有它，用户每 15 分钟就会被迫退出——没有 IM 应用能够通过这种测试。

**核心挑战**：
1. **拦截现有 API 请求而不重构** 200+ 个调用点。每个调用点都使用 `try/catch` 处理 `ApiError`，这意味着任何拦截器层都必须包装底层 `fetch` 调用。
2. **并发请求聚合**：当 10 个请求同时返回 401 时，只有一个应该触发 refresh；其他 9 个应等待第一个完成，然后使用新 token 重试。
3. **多标签页竞争**：两个标签页同时 refresh → 其中一个的 refresh token 会被另一个轮换失效。需要原子性 token 轮换（服务端已支持单次使用）。

**预期的架构变更**：
- 为 `request()` 引入一个内部中间件管线：`request → authMiddleware → rateLimitMiddleware → fetch`
- 在 `api.js` 中增加一个 `refresh` 函数和一个 `refreshPromise` 单例
- 添加一个可选的定期刷新计时器（每 10 分钟在后台刷新，避免 401 完全发生）
- WS token 需要在 refresh 后保持不变（token 在升级握手时验证，之后只检查 session）

**对现有系统的影响**：低。`api.request()` 是单一函数——添加拦截器逻辑而不改变任何外部接口。`forceReauth()` 保持不变，只是现在它是"最后手段"而非主要路径。

### 方向 B（P0）：带临时 ID 回显 + 超时的可靠消息交付契约

**为什么需要**：用户信任消息是否已发送。现在的交付不确定性（方向 2中的 3 种静默失败模式）会侵蚀这种信任。文件/语音发送永远无法匹配合适的服务器回显是一个存在性缺陷。

**核心挑战**：
1. **服务器回显 `tempId`**：需要一个新的 WS 帧字段（例如 `in_reply_to_temp_id`），服务器在 `Message` 帧中将其回显。这需要在服务器端的 `ClientFrame::SendMessage` 和 `ServerFrame::Message` 之间进行协调。
2. **超时+重试逻辑**：30 秒后，UI 必须将 pending 消息标记为失败，显示红色感叹号并允许手动重试。重试必须使用唯一的去重 key，这样服务器就不会创建重复消息。
3. **连接丢失后重连恢复**：如果 WS 在消息 pending 时断开，重连后客户端必须（a）重新发送 pending 消息，或（b）通过 REST 检查消息是否实际已被接收。

**预期的架构变更**：
- 为 WS `send_message` 帧增加 `client_temp_id` 字段（可选，服务器兼容忽略）
- 为 WS `message` 帧增加 `in_reply_to_temp_id` 字段（回显）
- 增加带指数退避的 `pendingQueue`：WS 不可用 → 消息进入队列 → 重连后自动重发
- `state.pendingByTempId` 增加超时属性 + 超时清理定时器
- 引入 `MessageDeliveryState` 枚举：`Sending | Sent | Failed { reason, retryCount } | Confirmed`

**对现有系统的影响**：中等。客户端的 WS 帧结构变化小；后端的变化也小（额外字段，可选）。主要的复杂度在客户端重试逻辑、超时管理和不可变 tempId 的引入。

### 方向 C（P1）：滚动位置状态机 + DOM 差异

**为什么需要**：房间切换时失去阅读位置是高端 IM 产品（Slack、Discord、Telegram）与其用户之间的一种信任破裂。这影响的是日常可用性，而非一次性体验。

**核心挑战**：
1. 在 DOM 重建（`replaceChildren()`）之前，必须从 `msgScroll` 捕获 `scrollTop` 并存到 `state.scrollPosByRoom`。
2. 在 DOM 重建之后，必须恢复 `scrollTop`，但前提是用户之前看到的是"历史中某处"——如果用户之前是在底部，应该保持底部（新消息向下滚动）。
3. 增量插入（历史加载向上翻页）需要一个滚动锚点：通常是第一条可见消息的 `data-msg-id`，这样当新元素插入到该消息上方时，客户端可以计算高度差并补偿 `scrollTop`。
4. 当用户"不在底部"时收到新消息 → 不应自动滚动；应显示一个指示器"↓ N 条新消息"。

**预期的架构变更**：
- `state` 增加 `scrollPosByRoom: new Map()`
- `switchRoom` → `rerenderCurrentRoom` 流程中增加保存/恢复钩子
- 对历史加载（向上翻页）的 `loadHistory` 使用锚定滚动补偿
- `appendMessageEl` 的行为取决于"是否在底部"

**对现有系统的影响**：低到中。渲染核心（`rerenderCurrentRoom`、`loadHistory`、`appendMessageEl`）需要调整，但没有全局的模式变化。

### 方向 D（P1）：系统化的重连状态恢复契约

**为什么需要**：虽然方向 4 的核心 claim（JoinRoom 不发送 Presence）是不成立的，但确实存在边缘场景：如果 `state.currentRoomId` 在断开期间被重置为 null，WS 重连就无法恢复房间上下文。此外，`replayChanges()` 的覆盖范围有限——它只重放编辑/删除，而不重新获取状态范围。缺少的是：WS 重连后的系统性能力恢复。

**真正的缺口**：
1. **`state.currentRoomId` 的丢失**：如果 JS 状态在断开期间被破坏（例如被 GC 回收、扩展冲突、意外的 `clear()`），则 `ws.on('open')` 会提前退出，不恢复任何内容。
2. **无手动刷新**：没有 REST 端点或 UI 按钮来拉取当前的在线列表——用户只能重新选择房间。
3. **无在线列表缓存**：如果 Presence 帧在重连后短暂丢失，UI 显示"0 在线"，直到下一个事件触发 presence 更新。

**核心挑战**：
- WS 重连必须与 REST 状态恢复相结合：重连后，验证 `state.currentRoomId` 是否与服务器的 live room 集匹配，如果不匹配则通过 REST 刷新
- `state` 需要更优雅的降级路径：如果 state 被破坏，自动回退到 REST 获取，而不是静默中断
- 应该有一个"会话健康检查"——一个每 30 秒的心跳，用于验证核心状态变量是否一致

**预期的架构变更**：
- `ws.on('open')` 在恢复房间上下文之前增加 `state.currentRoomId` 非空断言 + REST 恢复
- 为每个房间增加 `onlineByRoom: new Map()` 作为客户端缓存，在连接中断期间保留
- 可选：增加一个每 60 秒的定时器，通过 REST 重新验证房间状态

**对现有系统的影响**：低。主要是防御性编程和缓存改善。

### 方向 E（P2）：长期——声明式 UI 渲染策略

**为什么需要**：方向 2、3 和 4 都在不同程度上受到特设式 DOM 操作的约束。从"命令式 DOM 脚本"转向"声明式渲染"将系统性解决多个问题：滚动位置、pending 消息管理、增量更新。

**核心挑战**：
- **零依赖约束**（当前设计）：不能使用 React、Preact、Vue。但这并不意味着不能有声明式模式——`lit-html` 或简单的 `html` 模板字面量函数可以在 2KB 内实现基于 diff 的更新。
- **增量采用**：不能一次性重写整个 SPA。声明式渲染器必须能与现有命令式代码共存。
- **滚动锚定**：即使是声明式渲染器，也需要手动管理的滚动锚定——这不是免费的。

**预期的架构变更**：
- 引入一个轻量级的渲染助手（< 3KB），支持带键值的 DOM diff
- 逐步迁移 `rerenderCurrentRoom` → 声明式模板
- 声明式 pending 状态管理：`${pendingMsgs.map(m => renderMsg(m, {pending: true}))}`

**对现有系统的影响**：高（长期）。这不是一个几天的改动，而是一个季度的架构演进。应该在其他 P0/P1 项之后，作为专门的技术债务 Sprint 来处理。

---

## 3. 接口设计建议

### 请求拦截器接口（方向 1）

`api.js` 中 `request()` 函数的改进设计：

```
// request() 内部管线
request(method, path, opts) →
  1. authenticate(opts) → 注入 Authorization header
  2. fetch() 
  3. 如果 401 → tryRefresh() → 重试 fetch()
  4. 如果 refresh 失败 → 传播错误（调用者决定 forceReauth）
  5. 返回 data
```

关键设计约束：
- **无状态**：`request()` 不应改变 `auth` 单例，除非 `tryRefresh()` 成功
- **幂等**：多次 401 → 仅一次 refresh → 所有等待者共享同一个新 token
- **透明**：所有现有 `try/catch (err.status === 401)` 路径继续工作（但应该永远不会被触发）

### 消息确认契约（方向 2）

```typescript
// WS 消息契约
interface SendMessageFrame {
  type: 'send_message';
  room_id: string;
  blocks: Block[];
  reply_to?: string;
  client_temp_id?: string; // ← 新增：client 生成的唯一 ID
}

interface MessageFrame {
  type: 'message';
  message: Message;
  in_reply_to_temp_id?: string; // ← 新增：如匹配则回显 client_temp_id
}
```

客户端处理：
```
outgoing:   tempId = crypto.randomUUID()
            state.pendingByTempId.set(tempId, { message, timer: 30s })
            ws.send({..., client_temp_id: tempId})
            
incoming:   if msg.in_reply_to_temp_id exists →
              state.pendingByTempId.delete(msg.in_reply_to_temp_id)
              clear timer
            else →
              fallback to legacy textOf matching
```

对于向后兼容：旧服务端忽略 `client_temp_id`，旧客户端忽略 `in_reply_to_temp_id`。`textOf` 匹配作为后备继续有效。

### 滚动状态契约（方向 3）

```typescript
// state contract (internal only)
interface ScrollState {
  scrollTop: number;
  anchorMsgId: string | null;     // 第一条可见消息的 ID（用于增量插入）
  atBottom: boolean;              // 是否在底部（决定新消息行为）
}
```

`rerenderCurrentRoom` 应该：
```
saveScroll(roomId) → 
  state.scrollPosByRoom.set(roomId, {
    scrollTop: els.msgScroll.scrollTop,
    anchorMsgId: firstVisibleMsgId(),
    atBottom: isAtBottom()
  })

restoreScroll(roomId) →
  const saved = state.scrollPosByRoom.get(roomId)
  if saved.atBottom → scrollToBottom()
  else → els.msgScroll.scrollTop = saved.scrollTop
```

### 重连恢复契约（方向 4）

无新 API 端点需要——`JoinRoom` 处理程序已经发送 `Presence`。但客户端需要更有弹性的合约：

```typescript
ws.on('open', () => {
  // 步骤 1：验证核心状态完整性
  if (!state.currentRoomId || !state.me) {
    // 状态破损 → 通过 REST 恢复
    await restoreStateViaRest();
    if (!state.currentRoomId) return; // 没有任何活动的房间
  }
  
  // 步骤 2：加入房间 + 等待 Presence 确认
  ws.joinRoom(state.currentRoomId);
  
  // 步骤 3：恢复直播流
  for (const sid of state.watchedStreams) ws.watchStream(sid);
  
  // 步骤 4：重放变更
  replayChanges(state.currentRoomId);
  
  // 步骤 5：设置一个超时，如果 Presence 确认未出现则回退到 REST
  fallbackTimer = setTimeout(() => {
    if (onlineListEmpty()) refreshOnlineViaRest();
  }, 2000);
});
```

---

## 4. 技术选型

### 需要推倒重来吗？

**不需要。** 四个方向中有三个（1、3 和 4 的修正版）可以完全在现有架构内修复，无需新依赖。方向 2（可靠的乐观消息）需要服务端和客户端之间的 WS 数据契约的微小演进，但同样不引入新工具。

### 关于工具的具体建议

| 方向 | 需要新依赖吗？ | 评估 |
|------|---------------|------|
| 1 — Token refresh | 否 | 纯 JS，无外部依赖 |
| 2 — 可靠的消息交付 | 否 | 纯 JS + 服务端 WS 帧演进 |
| 3 — 滚动位置 | 否 | 纯 JS DOM API |
| 4 — 重连恢复 | 否 | 纯 JS 防御性编程 |
| E — 声明式渲染 | 可能（lit-html，~3KB gzipped）| 只在需要时才考虑 |

`lit-html`（方向 E）是唯一值得考虑的第三方依赖，而且只有在声明式方法被验证能解决足够多的问题，证明引入外部代码是合理的时候才考虑。它的约束范围符合零依赖精神：3KB gzipped，无虚拟 DOM 开销，可逐步采用。

### 自建 vs 采购

这里没有什么可"采购"的——这些是客户端架构的缺口，不是平台能力。唯一接近"采购"决策的是移动端推送网关的方向（方向 1 中的 refresh token），但 `Service Workers` + `Web Push API` 可以处理 Web 端的后台 token refresh，无需原生应用代码。

---

## 5. 实施路线图

### 优先级

| 优先级 | 方向 | 工作量估算 | 风险 | 依赖 |
|--------|------|-----------|------|---------|
| **P0** | 1 — Token refresh | 1-2 天 | 低 | 无 |
| **P0** | 2 — 可靠的消息交付 | 3-5 天 | 低（服务端变化小） | WS 帧演进 + 客户端重试逻辑 |
| **P1** | 3 — 滚动位置 | 2-3 天 | 低 | 无 |
| **P1** | 4 — 重连弹性 | 1-2 天 | 低 | 无 |
| **P2** | E — 声明式渲染 | 1 个 Sprint（迭代） | 中 | 所有 P0/P1 项目 |

### 阶段划分

**阶段 1（第 1-2 天）：修复存在性缺陷——Token refresh + 消息可靠性。** 用户无法"使用"产品（每 15 分钟被踢出 + 消息静默丢失）。这是"止血"阶段。

**阶段 2（第 3-5 天）：修复可用性缺陷——滚动位置 + 重连弹性。** 用户可以在产品中"工作"（切换房间不丢失上下文、重连不会破坏 UI 状态）。这是"恢复正常功能"阶段。

**阶段 3（第 6+ 天）：长期改进——声明式渲染策略。** 解决根本原因。可选——取决于 P0/P1 期间积累的技术债务承受能力。

### 详细阶段计划

**阶段 1a：Token refresh（第 1 天）**
1. 在 `api.js` 中实现 `tryRefresh()`：调用 `POST /api/auth/refresh` 并传入 `auth.getRefresh()`，更新 token
2. 在 `request()` 中实现拦截器：401 → `tryRefresh()` → 重试原始请求
3. 添加并发防护：`refreshPromise` 单例，等待中的请求共享
4. 添加定期刷新计时器（每 10 分钟）以减少 401 发生频率
5. 更新测试：验证 401 后不再触发 `forceReauth`，除非 refresh 也失败了

**阶段 1b：消息可靠性（第 2-3 天）**
1. 向 `ClientFrame::SendMessage` 和 `ServerFrame::Message` 添加 `client_temp_id` / `in_reply_to_temp_id` 字段（服务端：可选，向前兼容）
2. 客户端：在 `optimisticAdd` 中用 `tempId` 创建 pending 消息
3. 服务端：在确认中回显 `client_temp_id`
4. 客户端：收到匹配时清除 pending 定时器，替换 DOM
5. 对非文本消息添加基于 `tempId` 的后备匹配（无 `in_reply_to_temp_id` 时的传统 `textOf`）
6. 添加 30 秒超时 → pending 状态变为失败（红色感叹号 + 重试条目）
7. 添加带重试队列的 WS 发送失败路径

**阶段 2a：滚动位置（第 4 天）**
1. 向 `state` 添加 `scrollPosByRoom: new Map()`
2. 修改 `switchRoom` → 保存旧房间的 scrollTop
3. 修改 `rerenderCurrentRoom` → 恢复新房间的 scrollTop（如果存在）
4. 修改 `appendMessageEl` 和 `scroll` 事件 → 仅当在底部时才自动滚动新消息
5. 添加历史加载（向上翻页）的锚定滚动补偿：`firstVisibleMsgId`

**阶段 2b：重连弹性（第 5 天）**
1. 向 `state` 添加 `onlineByRoom: new Map()` 作为连接中断期间的 presence 缓存
2. 修改 `ws.on('open')` → 在恢复房间之前防御性地检查 `state.currentRoomId`
3. 添加 `onReconnect` 工作流（步骤 1：断言；步骤 2：加入房间；步骤 3：等待 Presence；步骤 4：回退到 REST）
4. 添加 Presence 确认超时 → 如果未出现则回退到 REST

**阶段 3（可选，第 6+ 天）：声明式渲染策略**
1. 评估采用 lit-html 或自定义 `html` 标签函数
2. 为 `rerenderCurrentRoom` 构建带键值的消息列表渲染
3. 将 `pendingByTempId` 集成到声明式 pending 状态中
4. 将 `scrollToMessage` 与声明式滚动锚定集成

### 风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|------|--------|--------|--------|
| Token refresh 的多标签页竞态 | 中 | 高—用户被踢出 | 服务端已支持单次 refresh token。客户端添加 refresh 轮换后的 `forceReauth` 作为后备 |
| 服务端回显中的 `client_temp_id` 被延迟 | 低 | 中 | 在 `findPendingMatch` 中保留传统 `textOf` 匹配作为后备 |
| 声明式渲染（阶段 3）增加 JS 包大小 | 中 | 低 | 保持 < 5KB；如果膨胀，放弃并坚持使用增量 DOM 修补 |
| 方向 E 的范围蔓延 | 中 | 中 | 明确约定：非重写——仅在当前架构内逐步演进 |
| 不完整的测试覆盖 | 高 | 中 | 新增一个 `tests/client/` 目录，把 WS 帧处理和 pending 状态转移动作加 smoke test（后端已有 `mock` 设施） |

### 成功标准

| 方向 | 可量化指标 |
|------|------------|
| 方向 1 — Token refresh | 连续会话时间超过 24 小时（access token TTL 的 96 倍）而无需重新登录 |
| 方向 2 — 消息可靠性 | 文件/语音/Markdown 消息的 pending → 已发送转换率达到 100%；失败消息在 30 秒内显示红色感叹号 |
| 方向 3 — 滚动位置 | 切换离开再回来时，阅读位置在 200px 以内 |
| 方向 4 — 重连弹性 | WS 断开并重连后，在 2 秒内恢复房间在线计数 |

---

## 总结

这不是一个"需要更多功能"的系统——这是缺少"存在性"的闭环。您已经构建了一个可靠的服务端（refresh token 生命周期、backfill、presence），但客户端从未连接这些端点。

架构上的修复是直接的，不需要新的框架或范式转变：

1. **请求管道拦截器**（方向 1）→ 让令牌自动刷新
2. **消息确认契约**（方向 2）→ 让消息交付可靠
3. **滚动状态机**（方向 3）→ 让房间切换不丢失上下文
4. **重连保护**（方向 4）→ 让重连具有弹性

方向 4 的 cross-validation 发现是正确的：`JoinRoom` 确实发送了 `Presence` 帧，所以主要的 presence 路径是通的。但真正的缺口是防御性的——`state.currentRoomId` 在断开后丢失的罕见情况，以及缺乏临时的 presence 缓存。

顺序：第 1-2 天解决 P0 项，第 3-5 天解决 P1 项，第 6+ 天评估渲染策略。
