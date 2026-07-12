现在我已经对所有关键声明进行了逐行交叉验证。以下是审核结果：

---

## 交叉验证结果报告

### 整体评估：证据扎实，需修正一处重要事实

该文档是一份经过充分研究、架构严谨的分析——代码证据引用的行号准确，推理逻辑严密。有一个重大事实错误需要修正，其余完全准确。

---

### ❌ 需要修正：方向一的状态表——`localStorage` 断言的错误

> **原文**: "统计：**全仓 Web SPA 零次 `localStorage**……唯一使用的持久化 API 是 `sessionStorage`"

**此条有误。** `web/api.js` 第 10-21 行大量使用 `localStorage` 来持久化 JWT 令牌：

```javascript
// web/api.js 第 10-21 行
getToken() { return localStorage.getItem(TOKEN_KEY); },  // TOKEN_KEY = 'aero_token'
getRefresh() { return localStorage.getItem(REFRESH_KEY); },
getPid() { return localStorage.getItem(PID_KEY); },
```

并且 `sessionStorage` 并非唯一的持久化 API——还有**两个持久化 API**，但用于完全不同、且与文档核心论题正交的目的。这一点应当标注承认，然后解释为何仍不影响论点：

- ✅ **JWT 令牌持久化**（`localStorage`）：这是身份认证凭证，属于标准安全实践，与**应用状态**（房间/消息/未读数）持久化无关。
- ✅ **AI 历史记录**（`sessionStorage`）：文档已正确提及这一条，并正确指出它是会话级（标签页关闭后即丢失）。
- ❌ **应用状态持久化为零**：房间列表、消息历史、未读计数、草稿、滚动位置等确实为零 `localStorage`/`IndexedDB`——**核心论点完全成立**。

> **建议修正**: 将 "零次 `localStorage`" 改为 "零次 `IndexedDB`、零次 `BroadcastChannel`、零次 `SharedWorker`；`localStorage` 仅用于 JWT 令牌，不做任何应用状态持久化"。并考虑在状态表中增加 `state.participants: Map<ParticipantId, Participant>`（context.js 第 42 行），它同样未被持久化。

---

### ✅ 方向一：SPA 状态持久化

**代码证据**：已验证——`web/context.js` 中确为纯内存的 `new Map()` 集合。`rerenderCurrentRoom()`（`app.js`）确实用 `msgList` 的替换操作从头渲染。`web/app.js` 第 138 行的 `pullRoomSince` 与第 176-189 行的 `handleResync` 均可独立验证。

**一处细节补充**：`app.js` 第 145-173 行的 `pullRoomSince` 不会检查通过 REST 拉取的消息 ID 是否**已存在于内存中**——这意味着如果在 Room 1 和 Room 2 中出现同样的消息，第二次拉取会导致重复，而 SeqGate 不会处理 REST 路径，仅在 WS 路径上生效。这与持久化论点正交，但属于完整性方面值得注意的地方。

---

### ✅ 方向二：跨标签页协调

**代码证据**：已验证。`web/context.js` 第 95 行的 `export const ws = new WsClient()`——每个标签页获取自己的 ESM 实例。`localStorage` 的 `storage` 事件、`BroadcastChannel`、`SharedWorker` 在整个代码库中均未使用。

**一处值得注意的细微之处**：`api.js` 中通过 `localStorage` 持久化的 JWT 令牌在第 10-12 行**被所有标签页共享**——同一个 origin 下的所有标签页读取同源 `localStorage`。这意味着如果标签页 A 登出（清除令牌），标签页 B 仍然运行着旧的 WS 连接，但**下次 REST 调用时** `auth.getToken()` 将返回 `null`。这恰恰印证了文档关于跨标签页未协调的论点，但同时也指向一个可能快速见效（quick-win）的发现：`localStorage` 的 `storage` 事件可以极低成本检测到跨标签页令牌失效。

---

### ✅ 方向三：WS 凭证生命周期

**代码证据**：已验证。`crates/aero-server/src/ws/ws_impl/mod.rs` 第 273-274 行：
```rust
let claims = match state.auth.verify(&p.token) {  // ← 唯一次验证
```
此后在第 305-364 行的 `run_socket` 循环中，不存在后续的 token 验证、定时重认证，也没有收到 `revoked_tokens` 通知后的断开连接逻辑。`frame.rs` 第 577 行的 `assert_room_access` 是对每条**传入动作帧**的即时访问审核，但从未重新验证过 token 本身的合法性。

**一处重要的架构性细微之处**：即使 token 被撤销，`assert_room_access` 在 `SendMessage` 路径上仍会检查 `revoked_tokens`（通过 `AuthUser`，而 WS 路径跳过该步骤）。这意味着：
- 一个已撤销 token 的用户**无法发送**新消息
- 但该**现有 WS 连接仍然保持活跃**
- 该用户**仍然接收**实时扇出的 `RoomEvent`（`Edited`、`Deleted`、`Reaction` 等）

---

### ✅ 方向四：WS 帧序列完整性

**代码证据**：已验证。`web/ws.js` 第 38-56 行的 `SeqGate`：
- ✅ 无缺口检测——`high`（第 53 行）被跟踪但从未用于缺口检测
- ✅ `SEQ_RECENT_CAP = 256`（第 17 行）— 活跃房间溢出会导致重复帧通过
- ✅ 无 `retransmit_request` 帧类型（全代码库搜索为零结果）
- ✅ 第 133 行的重新同步机制 `ws.on('msg:resync', () => handleResync())` 确实在 `web/app.js` 第 176-189 行触发，其行为与文档描述完全一致：对每个有状态消息的房间迭代，通过 REST 拉取数据，最多拉取 3 页。

**一处代码审查发现（不是文档错误，而是产品 bug 可能性）**：`handleResync`（`app.js` 第 181-189 行）在 `state.resyncInFlight` 为 false 时执行；但它会**串行**遍历所有房间。对于有 50 个活跃房间的用户，这最多会产生 50 × 3 = 150 次 API 调用。该函数没有超时处理，每个房间的循环中也没有 `catch`。这意味着如果第 5 个房间抛出错误（第 195-197 行的 `pullRoomSince` 内部有一个 catch，但 `handleResync` 第 181 行本身缺少 try-catch，第 189 行的 `finally` 会在错误抛出时释放锁，导致其他房间被跳过）。这实际上对应文档 "代价被低估" 的论断。

---

### ✅ 方向五：HLS 播放器韧性

**代码证据**：已验证。`web/live.js` 第 35-46 行：
```javascript
video.controls = true; video.playsInline = true; video.muted = true;
const src = `/hls/${s.id}/index.m3u8`;
if (window.MediaSource && video.canPlayType('application/vnd.apple.mpegurl')) {
    video.src = src;  // ← 没有任何错误处理
```

并且在代码库中搜索 `Hls.`（大写 H）没有结果——`web/index.html` 第 8 行加载的 hls.js CDN（`hls.js@1.5.18`，约 105KB gzipped 资源）**确实从未被 JavaScript 代码引用**。这句话百分百准确。

---

### 优先级评估：快速修正建议

| 方向 | 文档给出的优先级 | 我的评估 | 理由 |
|--------|----------|------|---------|
| 方向三（凭证） | **P0** | ✅ P0 | 合规性缺口——停用的用户保持实时连接。实际攻击面通过 `assert_room_access` 在写路径上得到部分缓解，但读/扇出路径完全没有防护。 |
| 方向一（持久化） | **P1** | ✅ P1 | 核心体验问题，已通过现有重连回传协议（`since` 游标 + `backfill` 框架）获得**部分**缓解，但 UI 状态完全丢失。 |
| 方向二（跨标签页） | **P1** | ⚠️ 降为 **P2** | 在实践中，方向一的重连回传会使标签页刷新在 2-5 次 API 调用后恢复，从而减轻"丢失未读计数"的痛点。在多标签页 Leader + BroadcastChannel 之前，未读计数的跨标签页同步可以通过 `localStorage` 的 `storage` 事件来廉价解决（令牌已经在那里了）。 |
| 方向四（帧序列完整性） | **P2** | ✅ P2 | 真实存在的可靠性缺口，但损失是"丢失了一条反应"而非"丢失了一条消息"。有消息 ID 的消息自有恢复途径。 |
| 方向五（HLS 播放器） | **P2** | ✅ P2 | 低成本投入（封装一个 `LivePlayer` + 复用 CDN 加载的 hls.js），在文档评估上做得对。 |

---

### 方向二降级的 "快速修正"

目前已有 `localStorage` 令牌 + `storage` 事件监听器——一个约 20 行代码的函数就可以跨标签页同步未读计数，无需投入完整的 BroadcastChannel 架构。思路如下：

```
1. 在 context.js 中: 在 setUnread(roomId, count) 中写入 localStorage.setItem(`unread:${roomId}`, count)
2. 在 app.js 初始化时: 监听 window.addEventListener('storage', ...) 来读取其他标签页的未读计数并合并
```

这是一个约 1 小时的低成本修复，可以解决方向二中最痛点（未读计数不同步），而完整的 BroadcastChannel + Leader Election 架构则继续在 P2 推进。

---

### 最终裁决

这是一份优秀的架构分析——代码引用准确，边界条件定义得当，与既有分析的区别划分清晰。唯一需要修正的事实性错误是 localStorage/JWT 问题。在将该文档加入分析目录之前，我建议修正多标签页讨论中关于持久化 API 调用的那条陈述。
