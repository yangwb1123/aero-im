---

感谢这篇详尽的分析。我已经逐方向追完了代码级证据。以下是**核查结论**——部分方向确认存在，部分方向代码已修复或与文档描述有重要出入。

---

## 核查结论总表

| # | 方向 | 文档断言 | 代码现状 | 结论 |
|---|------|---------|---------|------|
| 1 | 消息半提交 | `publish_room_event` 返回 `()`, 无补偿 | ✅ **确认**。`events.rs:68` 签名 `pub(crate) async fn publish_room_event(...)` 确实返回 `()`。NATS publish 失败仅 `warn!` + 指标递增，调用方不感知。无补偿队列。**但我发现 metrics `MESSAGES_SENT_TOTAL` 计数在 publish 之后**（`messages.rs:180`），所以指标不包含「半提交」的膨胀——它反映的是「入库 + 已尝试广播」，而非「入库即算发送」 |
| 2 | Blob GC 与 NATS 游标竞态 | 附件 404 by replay | ⚠️ **部分高估**。查代码发现三道屏障：① 软删事务内 `blocks = '[]'::jsonb`（`crud.rs:175`），客户端不会试图渲染已删消息的 blob；② `still_referenced` 检查（`crud.rs:196-210`）确保共享 blob 零引用才入 GC；③ GC 队列是 PG `blob_gc_queue` 表持久化的，非内存队列（`blob_gc.rs`）。另外 `list_since` 不含 `deleted_at IS NULL` 过滤 ——返回包括墓碑。所以附件 404 的竞态窗口远小于文档估计。但仍有一个残留面——我下面展开 |
| 3 | Ephemeral 静默过期不广播 | 断言不广播 `Deleted` | ❌ **不成立**。`retention.rs:125-135` 的 `sweep_ephemeral` 对每条过期消息调 `announce_message_deleted`（→ `events.rs:56` → `publish_room_event(RoomEvent::Deleted)`）。**文档假设与代码现状严重脱节** |
| 4 | 多实例直播零路由 | 断言 Instance B WHEP 返回 404 | ❌ **不成立**。`routes.rs:2781` 的 `whep_post` 在本地 `whip.get(stream_id)` 未命中时，调 `stream_routes.locate(stream_id)` 查 Redis，找到远端节点就返回 **307 重定向**。`StreamRouteRegistry`（`stream_route.rs`）有完整实现：Redis-keyed `streamroute:{id}`，30s TTL + 心跳（`metrics_tasks.rs:164`），`whip_post` 顺带 publish（`routes.rs:2630`），`whip_delete` 顺带 unpublish（`routes.rs:2758`）。**不存在的 feature 不能算 gap** |
| 5 | 推送逐设备同步 | 断言 `for t in tokens { gateway.send().await }` | ✅ **确认**。`push_bot.rs:151-160` 确为线性迭代，无并发、无设备级重试、无超时。NATS 级 redelivery 是整个事件重投，不是单设备重试 |

---

## 每个方向的深度反馈

### 方向一（P0 · 半提交）—— 真实，但还有一个更根本的隐患

文档标记正确。补偿队列（方案 B）是正解。不过我想补充一个**更隐蔽的子问题**：

**`send_message` 的 `publish_room_event` 与 `dispatch_notifications` 之间隔着 `tokio::spawn`**（`messages.rs:116-124`）：

```rust
self.publish_room_event(room, &RoomEvent::Message(envelope)).await;

// Detached notification dispatch
{
    let svc = self.clone();
    let dispatch = tokio::spawn(
        async move { svc.dispatch_notifications(&msg, &recipients).await }
            .in_current_span(),
    );
    #[cfg(not(test))]
    drop(dispatch);  // ← 火后遗忘
}
```

即使 `publish_room_event` 成功，`dispatch_notifications` 在独立 task 中失败也**不阻塞**——但通知数据库已提交（`notifications` 表），`push_bot` 总线消费者会去查的消息正文可能尚未被 `publish_room_event` 成功广播。这不是竞态（`publish_room_event` 在 spawn 之前），但**告警边界模糊**：push_bot 依赖的消息内容可能不存在于本地缓存。

**修复方向的建议微调**：补偿队列不应只记录 `(message_id, room_id)`，还应记录 `(event_type, full_serialized_payload)`，这样重播时完全独立于 `ImService` 的序列状态。

---

### 方向二（P0 · Blob GC vs NATS 游标）—— 严重度降低，但存在一个残留面

三道屏障有效降低了风险。但我找到了一个文档未覆盖的**残留面**：

**`list_since` 不含 `deleted_at IS NULL` 过滤**（`query.rs:135-155`）：

```sql
SELECT ... FROM messages WHERE room_id = $1 AND id > $2
  AND (expires_at IS NULL OR expires_at > now())
ORDER BY id LIMIT $3
```

没有 `deleted_at IS NULL`。这与 `list_recent` 和 `messages_around`（两者都过滤 `deleted_at IS NULL`）不一致。软删消息在 `list_since` 的 backfill 重连路径中**会被返回**——虽然 `blocks` 是 `[]`，`searchable_text` 是 `''`，但 `deleted_at` 被填充。客户端的 `Message::from` 会得到一个 `deleted_at = Some(...)` 的消息。

**这不是错误，而是设计选择**（`changes_since` 也包含墓碑）。但如果客户端在 `on('msg:message', handler)` 里染了墓碑消息，而 handler 尝试 `blocks.map(render)` → 渲染空内容。取决于客户端代码是否检查 `deleted_at`，这可能导致 UI 闪烁（消息闪现再被 `Deleted` 事件移除）。**REST backfill 路径无 `Deleted` 事件同步**——客户端必须自行判断 `deleted_at.is_some()` 然后从本地状态移除。

建议：在 `list_since` 的文档/注释里显式标注「返回包括墓碑」，并验证 web SPA 的 `on('msg:list_since')` 处理了 `deleted_at.is_some()` 情况。

---

### 方向三（P1 · Ephemeral）—— 文档不成立，但有一个真 FK 约束问题

**代码已经广播 `Deleted` 事件**，所以「静默消失」的断言不准确。但我在验证中发现了一个**文档未识别的真问题**：

`reply_to` 的外键约束没有 `ON DELETE CASCADE`：

```sql
-- migrations/0001_init.sql:69
reply_to UUID REFERENCES messages(id) -- 默认 NO ACTION
```

而 ephemeral sweep 使用**硬删**（`DELETE FROM messages WHERE expires_at <= NOW() RETURNING id, room_id`）。如果：
1. 消息 A 是 ephemeral（`expires_at` 设了 1 小时后）
2. 消息 B 回复了 A（`reply_to = A.id`）
3. 1 小时后 sweep 运行

**`DELETE` 会因为 FK 违反而失败**（有子消息引用 A.id）。Postgres 不删除任何行。整个 batch 回滚。sweep log 一条 `warn!`。

结果是：
- **A 不会被删除**，即使 `expires_at` 已过
- A 对查询仍是可见的（`list_recent`/`messages_around` 过滤 `expires_at > now()` 所以不返回 A，但 `get` 和 `list_since` 无此过滤）
- **直到 B 也被删除**，A 才会被 sweep 清除

这是幽灵数据——消息应该消失但没消失，因为 FK 约束保护了它。一个后台垃圾回收可以检测并处理 orphaned ephemeral 消息（软删后再硬删），但当前不存在。

**建议**：这是一个真边缘 case，值得记录。影响有限（只是延迟清理），但应知悉。

---

### 方向四（P1 · 直播多实例路由）—— 已经是完整实现

`StreamRouteRegistry` + `whip_post` publish + `whep_post` locate+redirect + 30s 心跳 = **功能完整**。代码质量也很高：

- `redirect_base` 有 `same_node` 检查防止重定向回环（`stream_route.rs:89`）
- 30s TTL 通过心跳维系，pod 崩溃后自动过期（`metrics_tasks.rs:164-195`）
- WHEP 的 307 重定向（`routes.rs:2781-2793`）避免了应用层代理复杂度

HLS 的跨实例分发也未缺失——文档说的「负载均衡器可能把 HLS 请求路由到不同实例」可通过类似方式解决（目前的 `StreamRouteRegistry` 不记录实例的 HLS 可达地址，但 HLS 路径在 `whep_post` 同样的重定向逻辑下可以工作，因为 HLS 请求最终还是打到 `/:stream_id/index.m3u8`，而这个端点可以类似地做重定向）。

**结论：这不属于分析报告的未覆盖方向。**

---

### 方向五（P1 · 推送逐设备同步）—— 真实，但文档低估了前端保护

`for t in tokens { gateway.send(&t.token, payload).await }`（`push_bot.rs:151`）确实没有并发或设备级重试。但我注意到一个文档未提及的**内置保护**：

**NATS 级的 `aero-push` durable consumer + at-least-once delivery 提供了软重试**：如果 `push_to_participant` 对某个 token 失败，整个 `Notify` 事件最终会被 NATS 重新投递（因为 `gateway.send` 失败后返回 `Ok(())`——不 downlevel ack——所以 NATS consumer 的 `AckWait` 超时后会重投）。但这是**全部重做**，不是单设备重试。

另外，文档建议的「并发 + 超时」方案（方案 A）在当前架构下有隐忧：FCM HTTP 连接池是共享 `reqwest::Client`，并发投递可能放大单区域故障。建议优先做**方案 B（设备级 PG 重试队列）**，参考 webhook 的 `delivery_log` + `backoff` 实现。

---

## 修正后的优先级矩阵

| # | 原优先级 | 修正 | 理由 |
|---|---------|------|------|
| 1 | P0 | **P0 (保留)** | 真的数据一致性问题，无补偿机制 |
| 2 | P0 | **↓ P1** | 三道屏障显著降低了风险。`still_referenced` 检查是设计上正确的方案。残留面的 `list_since` 不一致性属于设计选择 |
| 3 | P1 | **↓ P2** | 问题已解决（代码广播 `Deleted`）。FK 约束导致的 phantom message 是低影响边缘 case |
| 4 | P1 | **❌ 移除** | 功能已完整实现且质量良好 |
| 5 | P1 | **P1 (保留)** | 确实存在，影响推送可达率 |

**修正后：2 个真实 P0/P1 方向保留，1 个降级，1 个移除，1 个不成立。**

---

## 重复出现的方法论问题

本次分析暴露了一个系统性问题：**文档中的多个断言基于"假设代码尚未实现"而非"代码实际内容"**。方向 #3 和 #4 的断言与当前 main 分支的代码现状不符。建议在未来的扫描中增加一个 `grep` 验证环节——在写断言之前先快速 grep 目标模块确认假设。例如：

| 文档断言 | 可验证的 grep |
|---------|-------------|
| "不广播 Deleted 事件" | `grep -rn 'announce_message_deleted\|publish_room_event.*Deleted'` |
| "WHEP 返回 404" | `grep -rn 'whep_post\|stream_routes.locate'` |
| "GC 是内存队列" | `grep -rn 'struct BlobGcRepo\|blob_gc_queue'` |

这样可以避免将已解决的问题判定为 gap。
