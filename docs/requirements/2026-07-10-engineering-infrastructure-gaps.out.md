所有 5 个方向的关键主张均已通过代码验证。以下是结果：

## 代码验证结果

### ✅ 方向一：ESM 模块瀑布 — 已验证

`web/app.js` 确实通过原生 `import` 导入了 14 个模块（`api.js`、`calls.js`、`context.js`、`emoji.js`、`search.js`、`notifications.js`、`live.js`、`media.js`、`livecards.js`、`mentions.js`、`modals.js`、`auth_ui.js`、`chrome.js` + `ws.js`/`render.js` 由 `polls.js` 加载）。总 JS 传输体积 ~148KB（未压缩），18 个源文件，零 bundler。HTML 只引用 `app.js` + `polls.js` 两个入口，文件名不含 hash。分析准确。

### ✅ 方向二：Bus Ack 与错误安全网 — 已验证

```
crates/aero-server/src/ws/ws_impl/bus.rs:120  let _ = sub.ack().await;    ← 静默吞错误
crates/aero-server/src/ws/ws_impl/bus.rs:143  let _ = sub.ack().await;    ← 同上
crates/aero-server/src/ws/ws_impl/bus.rs:153  let _ = sub.ack().await;    ← 同上
crates/aero-server/src/ws/ws_impl/bus.rs:184  let _ = sub.ack().await;    ← 同上
crates/aero-server/src/ws/ws_impl/bus.rs:189  let _ = sub.ack().await;    ← 同上
```

全仓库 14 处 `sub.ack()` **全部**使用 `let _ = sub.ack().await`——零处检查错误。`room_member_cache.get_or_fetch()` 的 `unwrap_or_default()` 回落空列表意味着数据库瞬态错误导致消息**静默无人收到**。

### ✅ 方向三：WS 体面关闭 — 已验证

`shutdown.rs` 的完整流程：
```
shutting_down=true → sleep(drain) → ai_shutdown.cancel()
```
完全不与 Hub 交互。`Hub` 无 `shutdown()`、无 `close_all()`、无 `CancellationToken`。`register()` 和 `unregister()` 就是全部的连接管理。axum 的 graceful shutdown 仅停止接受新连接，已有 WS 被直接 drop。

### ✅ 方向四：事件发布原子性 — 已验证

`publish_room_event`（`events.rs:68`）是 fire-and-forget：
```rust
if let Err(err) = publish.await {
    warn!(?err, %subject, "publish RoomEvent failed");
    metrics::inc_counter(metrics::names::NATS_PUBLISH_ERRORS_TOTAL, 1);
}
```
无重试、无回滚、无发件箱。DB 写入在 bus publish 之前完成，publish 失败时消息已落地但无人能收到。

### ✅ 方向五：Hub 进程内状态无界 — 已验证

Hub 结构体与文档所述完全一致——5 个 `DashMap`，零上限常量。`register()` 无 `conns.len()` 检查。全部代码库搜索不到 `MAX_CONNECTIONS` 或 `MAX_ROOM_SIZE`。`join_room` 的 `contains` 守卫是 O(n) Vec 扫描（非 HashSet），存在竞争条件可能重复添加。

---

**结论**：这份分析准确反映了代码库中 5 个真实的工程基础设施缺口。文件 `docs/requirements/2026-07-10-engineering-infrastructure-gaps.md` 已存在于仓库。请问您希望我就此分析做什么？比如：

1. **实现 Sprint 1 的修复**（sub.ack 错误检查 + Hub 连接上限 + esbuild 集成）
2. **创建更详细的技术设计文档**（针对 Outbox 表或 WS 优雅关闭）
3. **进行额外的代码库探测**（确认某些建议方向的实施细节）
