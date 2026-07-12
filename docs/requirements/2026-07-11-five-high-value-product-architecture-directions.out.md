文件已存在于 `docs/requirements/2026-07-11-five-high-value-product-architecture-directions.md`。我已逐条验证代码锚点。以下是验证结果 + 深度分析。

---

## 代码验证结果

### 方向一（消息幂等）—— ✅ 准确，但有 1 处遗漏

**核心断言全部通过**：

| 断言 | 验证 |
|---|---|
| `ClientFrame::SendMessage` 无 `idempotency_key` | ✅ 确认。`ws_impl/mod.rs:72-80` 只含 `{ room_id, blocks, reply_to, expires_after_secs }` |
| `web/ws.js` sendMessage 无幂等键参数 | ✅ `sendMessage(roomId, blocks, replyTo=null)` — 无第 4 参 |
| `web/app.js` 有 `pendingByTempId` 乐观渲染 | ✅ `optimisticAdd` 生成 `_pending_<uuid>`，`findPendingMatch` 做临时映射 |
| 重连后无幂等重试防护 | ✅ 重连走 `?since=` 回放，但未确认帧不回放则重发无幂等键 |

**重要遗漏**：`StreamGift` **已有** `nonce` 做幂等键：

```rust
// ws_impl/mod.rs:195-200
StreamGift {
    stream_id: Ulid,
    gift_id: String,
    qty: Option<u32>,
    #[serde(default)]
    nonce: Option<String>,  // ← 这个 pattern 可以直接复用
},
```

这意味着 **幂等键模式在系统内已有先例**。`SendMessage` 加 `idempotency_key: Option<String>` 的改动不是发明新机制，而是把既有模式扩展到消息投递。REST webhook delivery 的 `record_attempt_is_idempotent_per_event` 测试（`webhook_delivery.rs:634`）也验证了 `ON CONFLICT DO NOTHING` 的可行性。

**但你遗漏了一个关键细节**：`SendMarkdown` variant（`ws_impl/mod.rs:96-102`）同样无幂等键。如果消息幂等要做，Markdown 路径必须同期覆盖——否则 `send_markdown` 成为重投绕过路径。

### 方向二（前端状态一致性）—— ✅ 基本准确，1 处需要校正

**核心断言全部通过**。

**需要校正**：你说 `"重连后不刷新在线成员名册"`。实际 `hookWs()` 的重连处理（`app.js:117`）虽然没有显式刷新，但 `joinRoom` 会触发服务端推送 `msg:presence` 帧。这意味着**被动刷新是有**的，但不是全量快照（增量 `{ room_id, online: Vec<id> }`）。问题是：如果离线期间成员加入/离开，用户 reconnect 后收到的 presence 增量可能不覆盖整个 gap 期间的状态变化。

你建议的 `sync_state` 全量快照协议是正确的方向——增量 presence 在重连场景有累积丢失风险。

`refreshReadStrips`（`app.js:382-440`）确如你所述基于 `state.receiptsByRoom` 渲染，而 receipts 通过 `GET /receipts` 懒加载。重连后不重拉 → 已读状态在长时间离线后确实漂移。

### 方向三（弹性限流）—— ✅ 基本准确，sweep_idle 需更细分析

**客户端忽略限流头**：✅ 确认。`web/api.js` 的 `request()` 对 429 只抛 `ApiError(429)`，不读 `Retry-After`，无自动退避。

**WS 重连退避不从服务端学习**：✅ 确认。`ws.js` 的 `BACKOFF_MS` 是硬编码 `[1000, 2000, 4000, 8000, 16000, 30000]`。

**`resync` 无 REST 速率控制**：✅ 但需区分实际情况。`handleResync()`（`app.js:180-190`）遍历所有 rooms 调用 `pullRoomSince(roomId, last, 3)`——每 room 取 3 条，不是全量。所以实际 REST 请求数 = rooms 数量。中等规模用户（50 rooms）产生 50 个并发请求，不算灾难但也不优雅。

**sweep_idle 耗尽 bucket 不回收**：⚠️ **需要更精细分析**。看代码：

```rust
// rate_limit.rs:181-195
let refilled = (b.tokens + elapsed.as_secs_f64() * self.rate).min(self.capacity);
// Keep while still rate-limited (not yet refilled) OR recently active.
refilled < self.capacity || elapsed < idle_after
```

**逻辑上**，一个在 `t=0` 耗尽的 bucket（`tokens=0`），经过 `capacity / rate` 秒后 `refilled = capacity`，然后如果 `elapsed > idle_after` 就会被回收。默认配置 `rate=10/s, capacity=100` 下，10 秒后虚拟满桶，默认 `idle_after=60s` → 70 秒后被回收。

所以**这不是永久内存泄漏**，而是**时间窗口放大**。恶意攻击者可以：
1. 每秒发送刚好 `rate × 0.9` 个请求，让 bucket 永远处于 `tokens < capacity` 状态
2. sweep 永不回收（`refilled < capacity` 一直为真）
3. 每个 attacker IP 占用一个 DashMap entry 直到不再发送请求后 60 秒

这是**低严重度 DoS 放大**而不是真正的内存泄漏。但方向是对的——sweep 应该增加一个 `min_idle` 硬上限参数，即使 `refilled < capacity`，如果 `elapsed > MAX_IDLE`（如 300 秒）也强制回收（fail-safe toward availability over enforcement）。

### 方向四（故障注入）—— ✅ 准确，数据支撑充分

- `tracing::error` 只在 4 个文件：✅ 确认（`background.rs`, `hub.rs`, `error.rs`, `worker/mod.rs`）
- 141 个 `tracing::warn` 调用遍布 fail-open 路径：✅ 确认
- 无故障注入测试：✅ 确认。819 测试全部是快乐路径
- `check_cluster_rate` 的 fail-open 无测试覆盖：✅ 确认

**一个有意思的发现**：SPA 前端实际上有**更多的** fail-open 逻辑。`web/` 下有大量 `catch { return; }` / `// best-effort` 模式：

```
web/api.js:123 — "truncated WS backfill / resync — ROADMAP v3 方向一"
web/app.js:150 — "best-effort: a failed catch-up page is retried on the next resync"
web/media.js — "Best effort" pattern in media uploads
web/polls.js — 推测也有（未完全验证）
```

前端 fail-open 是无测试盲区中 **最危险的**——因为 JS 的 `catch { return; }` 会静默吞掉所有异常，用户看到的是 UI 不更新但无错误提示。

### 方向五（媒体面 E2E）—— ✅ 准确

- `SfuMediaSession::bind()` / `run()` 仅在 `#[cfg(test)]` 实例化：✅ 确认（`sfu_media.rs` 注释已明文标注）
- `call_bridge_supervisor` 的 `ensure_egress` 标记 "已建+单测但生产未接线"：✅ 确认
- 无 HLS/SRT/WHIP E2E 测试：✅ 确认

---

## 跨方向关联与优先级修正

### 方向一 ↔ 方向四 的优先级已耦合

你标方向一为 P0、方向四为 P1。但事实上：

**消息幂等的 at-least-once 护栏只有在故障注入测试中才能验证正确性。** 因为：
- 幂等键的正常路径测试已经覆盖（`webhook_delivery.rs:634`）
- 真正需要验证的是「NATS 重投 + 消费方重启」组合场景下的幂等性
- 这需要故障注入（让 `ON CONFLICT` 分支被触发）

建议方向一（幂等键实现）和方向四（故障注入测试框架）**绑定为同一工作项的依赖关系**，而非独立优先级。

### 方向三（弹性限流）关于 429 响应 body 的建议需要权衡

你说「429 响应无结构化 body」。看代码：

```rust
// rate_limit.rs:264
response.headers_mut().insert("retry-after", HeaderValue::from_static("1"));
```

现在只返回 Axum 默认的纯文本。但你建议的 JSON `{ "error": "rate_limited", "retry_after": N, "limit": M }` 有两个问题：

1. **性能**：429 路径应该零分配。现在只设 header 就返回，序列化 JSON 增加 100+ 路径延迟
2. **客户端一致性**：既然 header 已经完整（`X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`, `Retry-After`），客户端只需读 header。缺少的是一个解析这些 header 的 `api.js` 中间件

更好的方案是：**保持纯 header 的 429，但在 `api.js` 加中间件统一解析 `X-RateLimit-*` header 并注入 `state.rateLimitStatus`**。body 格式不一致的 429 对客户端反而是累赘（多一个 JSON.parse）。

### 方向二（前端一致性）中 IndexedDB 缓存的风险

你建议 IndexedDB 做本地缓存。需要意识到：

1. **IndexedDB 跨 Tab 不共享**（同 origin 可共享但需 `BroadcastChannel` 同步）
2. **Storage 容量有限**（mobile Safari 约 50MB），消息体+附件元数据容易超过
3. **Service Worker + IndexedDB = 额外的 2 个概念负担**

对于 Aero IM SPA 当前架构（零 build 工具、纯 ES2020 模块），引入 IndexedDB + Service Worker 是架构级变更。**更务实的起点**：利用 `sessionStorage` + `localStorage` 缓存房间列表和已读游标（KB 级），消息历史依旧走 `?since=` 回放。

---

## 补充发现：2 个你未覆盖但相关的前端脆弱点

### 1. `findPendingMatch` 的时间窗口假设未参数化

```javascript
// app.js:482
if (Math.abs(sT - pT) <= 15000) return tempId;
```

临时消息和服务端回显匹配的时间窗口是硬编码 **15 秒**。如果用户网络延迟 >15 秒（不常见但移动网络可能），服务端回显的 `created_at` 与本地 `pending.created_at` 差值超过 15 秒 → 匹配失败 → 临时消息和真实消息重复渲染。这在消息幂等讨论中容易被忽略。

### 2. `hookWs()` 的 `ws.on('close')` 处理无指数退避与 jitter

```javascript
// ws.js — BACKOFF_MS 硬编码固定序列，无 jitter
const BACKOFF_MS = [1000, 2000, 4000, 8000, 16000, 30000];
```

当服务端重启（NATS + Hub 断连），所有客户端同时经过 `30 + 0ms`（最后一步 30000 毫秒无 jitter）后一起重连 → 恢复风暴。加 `+ Math.random() * 5000` 到 30s 步可消除。

---

## 总体评分

| 维度 | 评价 |
|------|------|
| 方向选择独创性 | ⭐⭐⭐⭐⭐ — 5 个方向在 324 份分析中无重复 |
| 代码锚点准确率 | ⭐⭐⭐⭐ — 1 处遗漏（`StreamGift.nonce` 先例）、1 处需更细（sweep_idle） |
| 技术深度 | ⭐⭐⭐⭐⭐ — 每个方向都摸到了具体代码行和接口定义 |
| 优先级合理性 | ⭐⭐⭐⭐ — 方向一 ↔ 方向四的依赖关系未标注 |
| 可操作性 | ⭐⭐⭐⭐ — 每个方向有具体代码修改建议，但 IndexedDB 建议高估了 SPA 架构承受力 |

---

## 建议的后续动作

1. **立即**：方向一实现 `idempotency_key` 字段到 `SendMessage` + `SendMarkdown`（复用 `StreamGift.nonce` 模式，约定 field 名统一为 `nonce`）。加迁移 `messages.idempotency_key` 列。这可以在 1-2 天内完成。

2. **同步启动**：`web/api.js` 加 429 中间件（解析 `X-RateLimit-*` header + 自动退避）。2 小时完成，与方向一无冲突。

3. **方向二务实第一步**：重连后发 `sync_state` WS 帧请求全量房间元数据快照，替代全量 REST refresh。这比 IndexedDB 实用得多。

4. **方向四第一步**：不要在 CI 加混沌测试——先找到所有 `tracing::warn` 的 fail-open 路径，逐一确认「丢什么、日志格式是否一致、on-call 能否收到告警」。这是零代码预算的审计工作。
