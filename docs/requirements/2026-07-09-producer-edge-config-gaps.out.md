文档已保存。以下是逐方向的代码验证报告：

---

## 验证结果

### 方向一（DB→NATS 崩溃窗口）— 核心观点 **有效**，但 1 处技术细节需修正

**✅ 核心观点成立**：`send_message` 中 `self.messages.insert(NewMessage{...})`（PG commit）与 `self.publish_room_event(...)`（NATS publish）之间没有事务边界。进程在这两步之间崩溃 → 消息持久化但永久丢失于实时总线。此窗口同样存在于 `edit_message`、`delete_message`、`toggle_reaction`、`mark_read` 等全部 ~15 个调用点。

**⚠️ 一处技术细节不准确**：文档称 `publish_bytes` 是 "fire-and-forget（`publish` vs `publish_with_ack`）"。实际上 `JetStreamBus::publish`（`crates/aero-bus/src/jetstream.rs:213`）已经 await JetStream ack——代码为 `let ack = self.js.publish(...).await?; ack.await.map_err(...)?;`。这是同步 publish-with-ack，不是 fire-and-forget。不过这不影响核心漏洞——即使 NATS publish 本身可靠，PG commit 和 NATS publish 之间的崩溃窗口依然存在。

**✅ 扩展方案合理**：Transactional Outbox（`event_outbox` 表 + `FOR UPDATE SKIP LOCKED` relay）是最低侵入的修复方案。

### 方向二（WS 优雅关停）— **全部成立**

**✅ 已验证**：`shutdown.rs` + `serve.rs` 的流程是收到信号 → `shutting_down=true` → sleep → `ai_shutdown.cancel()` → axum 返回 → TCP 关闭。全程没有任何 WS 通知帧广播。`Hub::fan_out_raw` 使用 `try_send`（非阻塞，丢帧不等待）。`background.rs` 中的 `run_bus_listener`/`run_live_bus_listener` 等后台任务收到 `CancellationToken` 但 WS 连接本身只等待 `graceful_shutdown`（只等 in-flight HTTP 请求完成，WS 被 RST）。

### 方向三（配置验证缺失）— **全部成立**

**✅ 已验证**：`crates/aero-server/src/bin/main.rs` 中唯一做配置验证的地方是 CORS 策略冲突（`AERO_CORS_REQUIRE_ORIGINS`）。全仓约 93 处 `std::env::var("AERO_*")` 散落在 10+ 个 crate 中。`AppConfig::load()` 通过 figment 只做类型级反序列化，不做值正确性验证。无 `--validate-config` 子命令。

### 方向四（扇出压力管理）— **全部成立**

**✅ 已验证**：`Hub` 已有 bounded mpsc（默认 capacity 256）、`disconnect_on_full` 模式和 `RESYNC_FRAME`。但：
- **无 per-connection 输出速率限制**（无 token bucket 机制）
- **无客户端 QoS 信号**（client → server backpressure）
- **无大房间扇出隔离**（所有房间共享同一 Hub 的 `DashMap` + mpsc，chatty 房间影响其他房间延迟）

### 方向五（API/WS 协议版本化）— **全部成立**

**✅ 已验证**：
- REST 路由全部挂载于 `/api/*`，无版本前缀
- `WsParams` 仅有 `{token, since, summarize, cursors}`，无 `version`/`capabilities`
- `RoomEvent` 使用 `#[serde(tag = "kind")]` **无 `deny_unknown_fields`**（与文档一致，旧客户端静默忽略新字段）
- `Block` 枚举同样使用 `#[serde(tag = "type")]` 无 `deny_unknown_fields`
- 无 `Sunset`/`Deprecation` 响应头

### 与既有文档无重复声明

经验证，5 个方向确实未被现有 179 份 `docs/requirements/` 文档覆盖。
