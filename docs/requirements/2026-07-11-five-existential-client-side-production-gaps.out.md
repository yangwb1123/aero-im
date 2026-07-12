---

## 交叉验证结果

我已经通读了这份文档（已在 `docs/requirements/2026-07-11-five-existential-client-side-production-gaps.md`），并对所有 4 个方向做了源码级定点核实。下面是验证结论：

---

### ✅ 方向一（Token refresh）—— **证据确凿**

**确认事实**：
- `api.js` 无 `refresh()` 方法，全局 grep `/auth/refresh` 在 `web/*.js` 中零命中
- `forceReauth()`（`app.js:975`）不检查 `auth.getRefresh()`，直接 `auth.clear()` → 退回登录页
- 所有 API 调用的 401 处理（`app.js:612`）一视同仁走 `forceReauth()`，无重试逻辑
- 服务端 `POST /api/auth/refresh` 端点完整实现（`session.rs`，含 `refresh_ttl_secs`），客户端存入 localStorage 但从不调用

**分析质量**：★★★★★ — 代码级证据链完整，边 case 考虑周全（并发 refresh、多 tab 竞争）

---

### ✅ 方向二（Pending 消息）—— **证据确凿，且比文档描述的更严重**

**确认事实**：
- `findPendingMatch`（`app.js:473`）的 `textOf()` 确实只提取 `type === 'text'` 块——文件/语音/卡片消息**永远无法匹配**
- `ws.send()`（`ws.js:164`）返回值在 `submitComposer()`（`app.js:905-906`）被丢弃
- `state.pendingByTempId` 无超时清理机制

**文档未覆盖的额外发现**：文档提到的"15 秒窗口"对**纯文本消息**也存在隐患——两条完全相同的文本在 15s 内先后发送，第二条的 pending 会错误替换第一条（`textOf` 和 `created_at` 都相同）。但 `crypto.randomUUID()` 生成的 `tempId` 是唯一的，所以替代方案可以基于 `tempId` 而不是文本匹配。

**分析质量**：★★★★★ — 证据链精确到位，三类静默失败分类清晰

---

### ✅ 方向三（滚动位置）—— **证据确凿**

**确认事实**：
- `switchRoom`（`app.js:533`）→ `rerenderCurrentRoom()`（`app.js:617`）→ `els.msgList.replaceChildren()` 完整重建 DOM
- `context.js:17-64` 的 state 定义中**没有任何** `scrollPosByRoom` 或类似字段
- `scrollTop` 的唯一使用（`app.js:592-595`）只用于历史加载补偿，不是房间切换
- `scrollToBottom()`（`app.js:795`）在每次房间切换后被直接调用

**分析质量**：★★★★★ — 代码路径追踪清晰

---

### ⚠️ 方向四（WS 重连 Presence）—— **存在重大不准确**

**这个方向的分析需要修正。** `frame.rs:46-54` 明确显示：**服务端在 `JoinRoom` 处理中必定发送 `Presence` 帧**：

```rust
// frame.rs:46-54
ClientFrame::JoinRoom { room_id } => {
    state.hub.join_room(room_id, pid);                    // 进程内注册
    state.presence.join(room_id, pid).await;               // Redis 集群注册
    let online = state.presence.members(room_id).await     // 读当前在线列表
        .unwrap_or_else(|| state.hub.room_members_online(room_id));
    tx.send(ServerFrame::Presence { room_id, online });    // ← 发送 Presence 回客户端
}
```

而客户端的 `ws.on('open')` handler（`app.js:97-104`）**确实调用了 `ws.joinRoom()`**：

```javascript
ws.on('open', () => {
    if (!state.currentRoomId) return;
    ws.joinRoom(state.currentRoomId);   // ← 这会触发服务端发送 Presence
    for (const sid of state.watchedStreams) ws.watchStream(sid);
    replayChanges(state.currentRoomId);
});
```

**完整的链路在 reconnect 场景下是通的**：`ws.on('open')` → `joinRoom` → 服务端查 Redis presence set → 回 `Presence` 帧 → 客户端 `handlePresence`（`app.js:491`）更新 UI。

`handleMembership`（`app.js:500-501`）的注释更是明确写了这个设计模式："re-join to pull a fresh Presence frame"——说明开发者已经考虑并实现了这个闭环。

**该方向文档错误的根本原因**：只看了客户端 `ws.on('open')` 的代码，没有追服务端 `JoinRoom` 的完整处理逻辑。文档说"不重新请求当前房间的在线成员列表"，但 `joinRoom` 本身就是这个请求。

**不过，文档有一个边缘场景是正确的**：如果重连时 `state.currentRoomId` 为 null（如页面在断开期间重置了状态），则 `ws.on('open')` 会在 `if (!state.currentRoomId) return;` 处提前退出，presence 不会恢复。但这是"JS 状态丢失"的问题，不是 WS 重连 presence 恢复机制缺失的问题。

**建议**：方向四应重写，改为真正的问题——**`state.watchedStreams` 在重连后被正确恢复（`app.js:101`），但 `state.currentRoomId` 若为 null 则整个 handler early return**。或者更精准的问题可能是"无独立的重连 online 刷新按钮/手动触发路径"。

---

### 汇总验证结论

| 方向 | 文档状态 | 验证结论 | 建议操作 |
|------|---------|---------|---------|
| 1 — Token refresh | 已存在 | ✅ 证据确凿 | 文件可保留，生成编码任务 |
| 2 — Pending 消息 | 已存在 | ✅ 证据确凿 | 文件可保留，生成编码任务 |
| 3 — 滚动位置 | 已存在 | ✅ 证据确凿 | 文件可保留，生成编码任务 |
| 4 — Presence 恢复 | 已存在 | ⚠️ 核心逻辑分析错误 | **建议重写**——服务端 `JoinRoom` → `Presence` 的路径是通的；应改为分析真正的缺口（如无手动刷新按钮、重连时 state 丢失后的降级路径） |

这份文档的 3/4 方向分析质量很高，方向四需根据服务端 `frame.rs:46-54` 的代码做修正。

需要我生成修正后的方向四内容，或者将已验证的三个方向转为编码任务清单吗？
