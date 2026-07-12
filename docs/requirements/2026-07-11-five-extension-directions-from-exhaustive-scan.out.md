好，以下是基于 12 个 crate 源码逐模块验证 + 与 ~130 份既有分析文档交叉对比的全面评审。

---

## 代码验证结果

### 方向一（P0）：生产者端消息投递可靠性

**大方向成立，两处事实需修正：**

✅ **核心论点成立**：`publish_room_event` 与 `PG commit` 之间**确实没有事务边界**。我已追踪全部 20 个调用点（你写 ~15，实际是 20），包括 `messages.rs`（109/197/232/267）、`reactions.rs`（78/108）、`reads.rs`（25/52）、`pins.rs`（50/71/96）、`channels.rs`（77/95）、`calls.rs`（51/104）、`orig.rs`（943/983/1065）、`events.rs`（57）。进程在 `messages.insert()` 提交后、`publish_room_event()` 完成前崩溃 → **消息永久丢失，无恢复机制**。这是真 P0。

❌ **"fire-and-forget" 指控不准确**：我在 `crates/aero-bus/src/jetstream.rs:213-222` 看到实际实现：

```rust
let ack = self.js.publish(subject.to_owned(), payload).await?;
ack.await.map_err(|e| BusError::Nats(e.to_string()))?;
```

这是 **JetStream `publish_with_ack`**——等待 NATS server 确认持久化后才返回。你的文档说"fire-and-forget（`publish` vs `publish_with_ack`）"，但实际已经在用 ack-wait 模式。问题不在于 `publish` vs `publish_with_ack`，而在于：

- PG commit 和 NATS publish 之间**没有原子边界**（这是真问题）
- 失败时只 `warn!` + `inc_counter`，**没有重试/补偿/发件箱 fallback**——这才是该方向的核心需要修正的表述

**与既有分析的重叠审查**：

| 文档 | 行 | 内容 | 重叠度 |
|------|-----|------|--------|
| `2026-07-02-scale-edge-security-analysis.md` | 76, 196 | "引入发件箱模式（outbox）：先写 event_outbox 表，后台 relay 投递到 NATS" | **高**——和你方向一的 outbox 提案**核心论点相同**（DB→NATS 双写），不是"consumer 侧补偿" |
| `2026-07-11-dual-write-and-consistency-gaps.md` | 方向一 | "消息发送路径的双写裂缝"——更加详细地分析了同一个问题 | **极高**——和你方向一几乎完全重叠 |

你文档中的声明"既有分析中'outbox'一词仅出现在 approvals.rs"和"scale-edge-security-analysis.md 论证的是 consumer 侧补偿"——**这两个声明与源码文档不符**。`scale-edge-security-analysis.md:76` 明确写的是 "避免进程崩溃导致「已入库但未投递」或「已投递但入库回滚」"，是**同一个 producer 侧问题**。

**建议**：将方向一从"全新发现"重新定性为"已有提议的深化——补充了更细的 crash 时间线、逐调用点分析、扩展选项（WAL 逻辑复制）和成本估算"。避免声称无重叠。

### 方向二（P0）：WebSocket 优雅关停

**全部成立，但已有部分提及。**

✅ **三个断层均被源码证实**：
1. **无 WS 关停帧**：`shutdown.rs:25-36` 只设置 `shutting_down=true` → sleep → `ai_shutdown.cancel()`，完全不接触 Hub 或 WS 连接。
2. **Hub 扇出不排空**：`hub.rs` 没有 `drain()` 方法，`fan_out_raw` 在关停信号触发后继续被 `run_bus_listener` 调用（因为 `run_bus_listener` 没有被 `ai_shutdown` 控制）。
3. **实例替换窗口**：`background.rs` 中 `run_bus_listener` 和 `run_live_bus_listener` 不被 `ai_shutdown` 控制——它们在新实例就绪前被独立 spawn。

**与既有分析的重叠**：
- `round-8-global-scan.md:258-266` 提到 "Phase C: 优雅关停系统性加固" 含 WS 通知
- `uncovered-expansion-directions.md:484-498` 提到 Hub draining
- `2026-07-10-post-exhaustive-scan-strategic-extensions.out.impl-plan.md` 有 "TASK-A03: WS draining close frame on SIGTERM"

但这些都只是**提及**，没有你文档级别的细粒度断层分析。你的方向二仍是**最完整的分析**。

**关键修正建议**：你的第二个断层（"Hub 扇出队列未排空"）最重要也最容易被忽略。但需要指出：当前 `hub.rs` 的 `fan_out_raw` 使用 `try_send`（非阻塞），所以问题不是"队列积压帧被丢弃"（这已经处理了），而是**关停期间 bus listener 仍在产事件扇出，但 WS 连接可能已被 RST**——`try_send` 碰到 `Closed` 会静默收走 stale 句柄，事件丢失且无人知道。

### 方向三（P1）：配置验证缺失

**✅ 全部成立，且确实无既有文档做系统分析。**

- CORS 验证是唯一存在的前置检查（`serve.rs:28-33`）
- 其他约 40 个配置项全部零验证——我验证了 `BLOB_DIR`、`HLS_DIR`、`ANTHROPIC_API_KEY`、`AERO_S3_*` 等散落在各处直接 `env::var()` 使用
- 但注意：`2026-07-12-tech-lead-implementation-plan-five-security-operational-directions.md:63` 已有 O2-003 "启动配置校验" 的实现计划，`architectural-blindspots-systemic-debt.md:269` 也提到 `--validate-config`

所以方向三的**分析是新的**，但**已经存在实现计划引用你的文档**（`producer-edge-config-gaps.md §3-5` 被多个 doc 引用为 source）。方向三在高价值上成立。

### 方向四（P1）：扇出压力管理

**✅ 主要成立，一个细分需澄清。**

✅ **a) 无 per-connection 扇出限流**：正确——`WsSender` 只有 bounded `mpsc`，没有 rate limiter。
✅ **c) 高流量房间影响其他房间**：正确——`hub.rs` 的 `fan_out_raw` 遍历所有 `recipients`，无 per-room isolation。
✅ **d) 直播弹幕无降级**：正确——`stream_watchers` 遍历所有 watcher。

⚠️ **b) 客户端信号机制**：技术上已有 `RESYNC_FRAME` 作为**服务端→客户端**的丢帧通知，但没有**客户端→服务端**的背压信号。这点需要明确区分。

**注意**：`architectural-gaps.md:225` 提到 "WS per-connection rate limiter" 但那是**输入**限流（对 `SendMessage` 等写操作），不是**输出**限流（服务端→客户端帧）。你的方向四聚焦在输出方向，这个角度确实是新的。

### 方向五（P2）：API/WS 协议版本化

**✅ 全部成立，分析质量高。**

a-d 四个缺口均被源码验证：
- REST 无版本前缀 ✅
- WS 无 version 握手 ✅（`handle_socket` 只认 `WsParams{token, since, cursors, summarize}`）
- `Block` 枚举无 `deny_unknown_fields` ✅（新 variant 静默破坏老客户端）
- Web 客户端无版本检查 ✅

与既有分析的重叠：`2026-06-29-codebase-analysis.md` 简单提到 `/api/v1/` 前缀（一行），你文档的 WS 协议版本化、枚举演化契约、能力协商三个子维度完全没有被覆盖过。**这是 5 个方向中唯一真正零重叠的方向。**

---

## 修订建议

### 需要修正的事实错误

| 位置 | 原文 | 修正 |
|------|------|------|
| 方向一·现状 | "当前 `publish_room_event` 中的 `bus.publish_bytes` 为 fire-and-forget（`publish` vs `publish_with_ack`）" | 实际是 JetStream `publish` + ack await ——不是 fire-and-forget。失败不在发布时而在 ack 超时/错误时 |
| 方向一·扩展 3 | "切换为 `publish_with_ack`" | 已经是 ack-wait 模式。应改为"在 ack 失败时写回 outbox" |
| 方向一·影响表 | "其他实例的 `run_bus_listener` 永远收不到" | 删去"永远"——严格说是"直到崩溃恢复且无自动重发机制之前" |
| 无重复声明 | "方向一在 scale-edge-security-analysis.md 中被提及但聚焦点不同（consumer 侧补偿事务 vs producer 侧崩溃窗口）" | 该文档：76 行明确写的是"避免进程崩溃导致「已入库但未投递」或「已投递但入库回滚」"——和你**同一个**双写问题，都是 producer 侧。应承认重叠并说明你的深化在何处 |

### 值得补充的细节

1. **方向一**：目前 `publish_room_event` 失败时的行为是 `warn!` + `inc_counter`，没有 `RETRY`。而 NATS 连接有自动重连（`async-nats` 内置），所以短暂网络闪断不会失败——只有 `ack.await` 超时（默认 30s？）或 `publish` 调用时连接已永久断开才会。这意味着窗口比你描述的小，但也更难触发更难排查。

2. **方向二**：应检查 `run_bus_listener` 是否被 `ai_shutdown` 控制。从 `background.rs` 看，它是 `tracker.spawn` 的，但没有 `select!` 监听 `ai_shutdown`——这意味着关停流程中 bus listener 仍在跑，直到 `TaskTracker.close()` 后 `tracker.wait()` 超时才停止。这正是你描述的问题。

3. **方向四**：可以补充一个度量——当前 bounded `mpsc` 默认容量 256。对于 100 msg/s 的房间，缓冲约 2.5 秒。但对于直播弹幕高峰（可能 1000+ msg/s），256 只够 250ms。应该建议 `send_queue_capacity` 的动态调整或按房间类型设定不同大小。

### 优先级微调

| 方向 | 你的推荐 | 建议调整 | 理由 |
|------|---------|---------|------|
| 一·Producer outbox | **P0** | 维持 P0 | 数据完整性裂缝。但实施成本要从"2-3 人周"上调到 3-4 人周（需要额外考虑 outbox relay 与现有 AiWorker 预算系统的协调） |
| 二·WS draining | **P0** | **降 P1** | 影响面大但触发窗口窄：只在滚动更新/缩容时发生。与方向一（每种消息都有风险）不同——方向二真正伤害发生在部署窗口（> 分钟级）而非运行时（> 毫秒级） |
| 三·Config validation | **P1** | 维持 P1 | 成本极低（1 人天 vs 你写的 1 人周），ROI 极高 |
| 四·Fan-out pressure | **P1** | 维持 P1 | 在大房间/大直播场景是关键；但大多数房间 < 50 人不会触发 |
| 五·API versioning | **P2** | 维持 P2 | 越晚越贵，但当前客户端唯一，不是阻塞项 |

---

## 总结

你的文档在 5 个方向中有 **4 个提供了有意义的增量价值**（比既有分析更深/更全/更代码级），有 **1 个存在高重叠但仍是更好的表达**（方向一——`scale-edge-security-analysis.md` 更早提出 outbox，但你的方向一提供更细的 crash 时间线、逐调用点枚举和扩展选项比较）。

方向五（API/WS 协议版本化）是**真正零重叠的原创分析**——建议在修订版中以它作为"无重复"的主要代表。

两个事实错误需要修正（`fire-and-forget` 和 `scale-edge-security-analysis` 的定性），但整体分析质量高、代码证据扎实、扩展方向务实。建议出 v2 版本修正上述两点后归档到 `docs/analysis/` 而非 `docs/requirements/`（后者看起来是草稿区，「正式分析」进 `docs/analysis/`）。
