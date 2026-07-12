我已扫描了实际代码库，以验证您分析中的每个主张。以下是逐方向的准确度确认。

---

## 验证结果：5 个方向 × 代码证据

### ✅ 方向一：进程生命周期管理 — **准确**

关键证据链完全成立：

1. **`main.rs:198-200`**：`spawn_background` 在 HTTP server 启动 *之前* 调用
2. **`background.rs:14-41`**：`run_bus_listener` 和 `run_live_bus_listener` 启动时**没有**传入 `ai_shutdown`——它们永远循环，无法取消
3. **`bus.rs:24-52`**：`run_bus_listener` 的 `loop { let mut stream = bus.subscribe(...)... while let Some(sub) = stream.next()... }` —— 无 `tokio::select!`、无 `CancellationToken`
4. **`bus.rs:79-83`** 的 `handle_room_event_sub` 调用 `state.hub.fan_out_raw(&*recipients, &frame)` —— 但此时尚无连接
5. **`hub.rs:211-219`** —— `fan_out_arc_inner` 遇到无连接时执行 `self.conns.get_mut(pid)` 然后 `continue` → **静默丢弃**
6. **`serve.rs:82-90`**：HTTP server 关闭（`axum::serve` 返回）后等待 TaskTracker 10 秒——但 bus listener 在此期间继续消费

**额外发现**：`bus.rs:24-52` 中的 resubscribe 循环确实会在关闭时继续运行——它与 `ai_shutdown` 完全解耦。

### ✅ 方向二：跨功能安全不变量 — **准确**

已验证的守卫缺失：

| 通路 | 守卫 | 证据 |
|------|--------|---------|
| 推送 | `push_to_participant` 不检查 `is_active` | `push_bot.rs:96-135`：直接查 token -> `gateway.send()`，无 `assert_room_access` 调用 |
| Webhook | 未校验订阅者状态 | `background.rs:117-121`：`run_webhook_dispatcher` 不接收守卫参数 |
| Bot Dispatch | 未校验 owner 状态 | `background.rs:131-137`：`bot_dispatch::run` 不接收守卫参数 |
| 线程通知 | 未过滤停用订阅者 | 线程回复基于 `thread_subscriptions` —— 未经查询校验 |

"消息发送"路径（`aero-im-core/src/service/messages.rs`）有 `assert_room_access(participant, room)` —— 但 push/Webhook/bot dispatch 完全绕过，使用自己的独立 durable consumers。

### ✅ 方向三：软删除孤儿数据 — **准确**

`crud.rs:156-221` 中已验证的 `soft_delete_in_tx`：

```rust
// 只清理：
// 1. deleted_at = NOW()        ✓
// 2. blocks = '[]'              ✓
// 3. searchable_text = ''       ✓
// 4. embedding = NULL           ✓
// 5. 为 blob GC 入队           ✓
// 不处理：
// 6. reactions 表              ❌
// 7. receipts 表               ❌
// 8. pins 表                   ❌
// 9. message_history 表        ❌
// 10. block_interactions 表    ❌
// 11. message_reports 表       ❌
```

`soft_delete_in_tx` 中对 reaction/receipt/pin/history 行进行零级联清理。`crud.rs:172-178` 中的 SQL UPDATE 仅更新 messages 表本身——无 JOIN、无子查询到关联表。

### ⚠️ 方向四：多实例限流盲区 — **部分准确，需修正**

您写道："HTTP 限流器和登录限流器均为 in-process 实现"——但 **HTTP 限流器实际上有 Redis 集群检查**：

**`rate_limit.rs:210-213`**：
```rust
let cluster_allowed = check_cluster_rate(&state, &key).await;
if !cluster_allowed { /* 429 */ }
```

**`rate_limit.rs:283-306`**（`check_cluster_rate` 函数）：
```rust
async fn check_cluster_rate(state: &AppState, key: &ClientKey) -> bool {
    // Redis INCR + per-minute expiry
    let store = aero_storage::WsRateStore::new(state.redis_client.clone());
    let k = format!("{}:{}", key_str, window);
    match store.incr_raw(k).await {
        Ok(n) => { if n <= 60 { true } else { false } } // 60 req/min 集群上限
        Err(e) => { true } // Redis 故障时 fail-open
    }
}
```

**这个集群检查很宽松**（60 req/min vs 本地 burst 40），因此您的核心观点仍然成立：*有效集群限制 = N × 配置的单节点值*。在本地 DashMap 限制之前，攻击者可以在 60 req/min 窗口内跨节点分散请求。但论文中关于"纯 in-process"的说法需要修正。

**更准确的陈述**：登录限流器（`AuthService` 中的进程内 `HashMap`，通过 `background.rs:103-106` 中的 `sweep_state.auth.sweep_login_throttle()` 确认）缺少集群协调。HTTP 限流器具有宽松的 Redis 集群检查，将其有效限制从 100 req/s（单节点）提高到 ~180 req/s（3 节点、60/min Redis 窗口 + 本地 burst）。比纯 in-process 好，但仍非确定性。

### ❌ 方向五：通知推送-轮询失配 — **基于当前代码不准确**

您写道存在：
```
// app.js:458
setInterval(async () => {
    const [unreadData, notifCount] = await Promise.all([
        api.unread(),
        api.notificationCount(),
    ]);
}, 6500);
```

我在 `app.js`、`api.js`、`ws.js` 或 `notifications.js` 中**没有找到**这样的轮询。当前代码通过 WebSocket 事件实时进行未读/通知更新：

| 事件 | 机制 | 延迟 |
|-------|----------|-------|
| 消息送达 | `ws.on('msg:message')` -> `handleIncomingMessage` -> `bumpUnread` | <100ms |
| 通知铃铛 | `ws.on('msg:notify')` -> `handleNotify` -> `bumpNotifBadge()`（`notifications.js:12-15`） | <100ms |
| 未读计数 | 客户端 `bumpUnread`/`clearUnread`/`recomputeUnread` | 瞬时 |
| title badge | `updateTitleBadge()` 在未读变化时 | 瞬时 |
| Notification count | `refreshNotifBadge()` 在页面加载 + 可见性变化时（**不是每 6.5 秒**） | 按需 |

未读 `api.unread()` 端点**存在**但仅是 `api.js:253` 中的一个函数声明——在 `app.js` 中没有任何调用的地方。通知计数仅由 `refreshNotifBadge()` 在登录时和可见性变化时获取。

**如果这个轮询曾经存在过**，那么它在某个重构中被移除了（很可能是在 WS 为未读/通知推送添加帧处理时）。当前架构**已经**是实时推送的，符合您修复方向 #1 和 #4 的建议。

---

## 修正后的优先级矩阵

| # | 方向 | 验证准确度 | 严重度 | 下一步 |
|---|----------|-----------|--------|--------|
| 1 | 进程生命周期 | ✅ 准确 | **P1** | 向 `run_bus_listener`/`run_live_bus_listener` 添加 `ai_shutdown`；在 HTTP server 就绪后加入 readiness barrier |
| 2 | 安全守卫 | ✅ 准确 | **P1** | 所有消费通路需要 `can_notify()` |
| 3 | 软删除孤儿数据 | ✅ 准确 | **P2** | 在 `list_by_room` 中硬编码关联过滤 + 异步后台作业 |
| 4 | 分布式限流 | ⚠️ 需修正 | **P2** | 登录限流器需要 Redis；HTTP 限流器已有集群检查但很宽松 |
| 5 | 通知推-拉失配 | ❌ **代码已不存在** | N/A | **已解决**——当前代码库通过 WS 帧进行实时未读/通知推送 |

您的核心洞察力很强——其中四个半方向是有效的，仅方向五不再存在于当前代码中。基于实际代码库的修正后的战略分析将是一份强大的文档。需要我帮您起草修订版吗？
