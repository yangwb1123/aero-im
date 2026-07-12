Good — I've done extensive code verification. This is a superb analysis with exceptionally high signal-to-noise. Let me give you a **verified/unverified scorecard** per directional claim:

---

## 代码验证结论

### 方向一 · Webhook/Bot Crash-Safety: ✅ 核心主张全部成立

| 主张 | 结果 | 源码行 |
|------|------|--------|
| `claim_due` 只查 `status='failed'` | ✅ **精确成立** | `webhook_delivery.rs:319` |
| `record_attempt` 插入 `status='pending'` | ✅ 成立 | `webhook_delivery.rs:196` |
| Stuck pending 行永不被回收 | ✅ **生产裂缝确认** | claim_due 查询条件不覆盖 `pending` |
| 幂等索引阻止重投重复 insert | ✅ 成立 | `record_attempt.rs:202` — `ON CONFLICT (webhook_id, event_id)` |
| Bot 交付无清理 | ✅ 成立 | `bot_subscription_deliveries` 表（迁移 0147）只有 write-only log，无 sweep |
| 跨节点 retry 真空 | ✅ 成立 | retry loop 是 `tokio::spawn` 单节点任务，无 leader election |

**修正建议**：`webhook_delivery_log` 确有 `sweep_terminal_before`（line 165），且 retention sweep 中 `sweep_webhook_logs` 调用了它。但只能清理 `delivered`/`dead`——**仍不触及 stuck pending**。建议强调「pending 和 failed 行永远不被 sweep」，而非说「webhook 没有 cleanup」。

---

### 方向二 · NATS 单集群假设: ✅ 所有主张成立

| 主张 | 结果 | 源码 |
|------|------|------|
| Subject 扁平命名 `im.room.{id}` | ✅ 成立 | `orig.rs:632` — `format!("im.room.{room}")` |
| 无区域前缀 | ✅ 成立 | 全部 bus subject 无 region 维度 |
| 8 个 durable consumer 绑定同一集群 | ✅ 成立 | bus.rs + agent_bot + ooo_bot + unfurl_bot + transcribe_bot + push_bot + moderation_bot + golive_bot |
| 无 NATS 故障逃生 | ✅ 成立 | bus.subscribe 失败只重试，无 fallback |
| Consumer cursor 跨 region 不共享 | ✅ 成立 | 验证了 `delivery_cursors` 表（迁移 0153）的查询路径 |

---

### 方向三 · 存储无界增长: ⚠️ 核心成立但有 1 处不精确

| 主张 | 结果 | 源码 |
|------|------|------|
| `message_history` 无清理 | ✅ 成立 | 不在 retention sweep 中 |
| `block_interactions` 无清理 | ✅ 成立 | 迁移 0086 只有 CREATE TABLE，无 cleanup |
| `bot_delivery_log` 无清理 | ✅ 成立 | `bot_subscription_deliveries` 是 append-only log |
| `reactions` 无清理 | ✅ 成立 | 不在 retention sweep 中 |
| `message_reports` 无清理 | ✅ 成立 | 不在 retention sweep 中 |
| `webhook_delivery_log` 有 `cleanup_old` | ⚠️ **函数名是 `sweep_terminal_before`，已在 retention.rs 中调用** | `webhook_delivery.rs:165` + `retention.rs:232` |
| `message_history` 与 `retention_days` 无联动 | ✅ **这是真的缺口** | `sweep_messages` 只删 messages 主表，不删 `message_history` |

**修正建议**：
- 函数名 `cleanup_old` → `sweep_terminal_before`
- 补充说明：`sweep_terminal_before` 已接入 `retention.rs`（`sweep_webhook_logs`），默认保留 30 天
- 最佳 find：`message_history` 的 retention 脱钩——messages 过期后编辑历史永存

---

### 方向四 · 总线消费者韧性: ⚠️ 核心成立但遗漏 resync 机制

| 主张 | 结果 | 源码 |
|------|------|------|
| Consumer 启动无依赖健康探测 | ✅ 成立 | `bus.rs` subscribe 只 retry，不健康检查 PG/Redis |
| `fan_out_raw` 丢帧 | ✅ 成立 | `hub.rs:347` — `try_send` 返回 `Full` 时 drop |
| ack 发生在 fan_out 之后 | ✅ 成立 | `bus.rs:143` — `state.hub.fan_out_raw` + `sub.ack()` 无条件 |
| 坏 payload nack 无上限 | ⚠️ **实际上代码是 ack-drop poison，不是 nack** | `bus.rs:171-173` — 两阶段解码都失败时 **ack**（注释说 "redelivery will never succeed — so ACK-drop"） |
| `agent_bot` 同步 AI 阻塞 consumer | ✅ **成立，这是真实问题** | `agent_bot.rs:47-52` — `handle` await AI，阻塞 `while let Some(sub) = stream.next()` |
| `moderation_bot` 用 bounded queue 隔离 | ✅ 成立 | `moderation_bot.rs:64-70` — mpsc queue + N workers |

**修正建议 ⚠️ 重要**：
- `fan_out_raw` drop 并非静默——代码有 `lossy` 旗标 + `RESYNC_FRAME` 恢复机制（`hub.rs:334-370`）。慢消费者丢掉一段时间后发送一条 `{"type":"resync"}` 帧触发 REST `since=`
- 文档说 "它仍然 ack" 精确；但说 "消息被静默丢弃" 不完全——resync 机制会让客户端自动补回
- 坏 payload 行为是 ack-drop poison，不是 nack-loop——**实际上比文档描述的更安全**（文档猜测 nack 会导致无限重投，但实际代码用 ack-drop 避免了这个问题）
- 可补充：`agent_bot` 这确实是性价比高的修复点——改成 `moderation_bot` 的 bounded queue 模式

---

### 方向五 · 媒体管线可观测性: ✅ 所有主张成立

| 主张 | 结果 | 源码 |
|------|------|------|
| SFU forwarder 无 Prometheus metrics | ✅ 成立 | `sfu_forwarder.rs` — 零 metrics 引用 |
| 无延迟直方图 | ✅ 成立 | 全 crate 无 `histogram!` 调用 |
| 无流健康 API | ✅ 成立 | 无 `GET /api/streams/:id/health` |
| 无通话质量 Scorecard | ✅ 成立 | `call_recap.rs` 无质量指标 |
| 无跨节点桥接可观测 | ✅ 成立 | `call_bridge.rs` 无 metrics |

**唯一补充**：RTCP 模块有内部 `counter`（`rtcp_fb.rs:149`— `Feedback packet counter`），但**不是 Prometheus 导出**，只是代码注释中的说明。结论不变。

---

## 综合评估

| 维度 | 结论 |
|------|------|
| **覆盖度** | 5 方向均在既有 60+ 分析中零命中 ✅ |
| **代码证据** | 〜90% 主张精确匹配源码 |
| **技术深度** | 非常高——`lossy`/`RESYNC_FRAME` 级细节是只有逐行读码才能发现的 |
| **需修正 (3 处)** | ① 坏 payload 实际是 ack-drop 非 nack-loop（方向四 B）② fan_out_raw 有 resync 机制（方向四 C）③ webhook cleanup 函数名 + 已接入 retention（方向三） |
| **最有价值的发现** | Section 1 stuck pending + Section 4 agent_bot 阻塞 + Section 3 message_history 脱钩 |

建议在最终版修正上述 3 个准确性问题。尤其是方向四的两个修正——ack-drop poison 设计说明团队已经考虑过 nack-loop 问题（比文档的推测更成熟），`lossy`+`RESYNC_FRAME` 也是高质量的慢消费者处理方案。这些修正不会削弱你的论点，反而证明你的扫描精度已经达到了能发现细微差距的水平。
