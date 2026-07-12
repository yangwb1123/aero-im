所有关键锚点均已验证。以下是综合评估。

---

# 审查：Aero IM 架构扫描文档

## 总体评价

这是一份**非常出色**的分析文档。与源码的对照验证显示，五个方向的论断基本准确、代码锚点精确、问题描述贴合实际工程契约。作者显然做了深入的代码阅读工作。

我从三个维度评估：**事实准确性**、**分析深度**、**实用价值**。

---

## 一、事实准确性审计

| 文档主张 | 验证结果 | 备注 |
|---------|---------|------|
| **方向一**：共享 `PgPool`，固定 `max_connections`，无运行时调整 | ✅ 完全准确 | `state.rs:244` `pub pg: PgPool`；`persistence.rs:31` 固定配置 |
| AI worker 与热路径共享池 | ✅ 准确 | AI worker 用 `Semaphore` 限并发（默认 50），但池是同一份 |
| 消息发送路径无事务包装 | ✅ 准确 | `messages.rs` 的 `send_message` 是串行 await 调用，无 `tx.begin()` |
| metrics_tasks 仅写 gauge，不反馈 | ✅ 准确 | `metrics_tasks.rs` 只有 `set_gauge`，无回调到限流器 |
| **方向二**：`ws.send()` 返回 `false` 未检查 | ✅ 准确 | `ws.js:169` 返回 `false`，`app.js` 中的 `sendMessage()` 不做检查 |
| 无服务端确认机制 | ✅ 准确 | 无 `MessageAck` 帧，`msg:message` 处理用隐式 sender_id 匹配 |
| `publish_room_event` 失败仅 `warn!` | ✅ 准确 | `events.rs:68` 确认 |
| **方向三**：限流器用内存 DashMap | ✅ 准确 | `rate_limit.rs` 是 `Arc<DashMap<ClientKey, Bucket>>`，不依赖 Redis |
| spam_guard Redis 失败 fail-open | ✅ 准确 | `spam_guard.rs` 中 Redis 路径的 `Allow` 回落 |
| NATS 订阅无限重试无熔断 | ⚠️ 部分准确 | `bus.rs` 的 `loop { subscribe; sleep(1s) }`，退避是恒定的 1 秒——有退避但无上限退避或熔断状态机 |
| **方向四**：三层限流全静态 | ✅ 准确 | `RateLimitConfig`、`WsRateTier`、`SpamThresholds` 全静态 |
| **方向五**：无帧尺寸校验 | ✅ 准确 | `WsConfig` 只有 `send_queue_capacity` + `disconnect_on_full`，无 `max_frame_bytes` |
| HTTP 有 `max_body_bytes` WS 没有 | ✅ 准确 | `config.rs` HTTP `max_body_bytes: 33MiB`，WS 无对应项 |
| `Block::Voice` 含大 base64 数据 | ✅ 准确 | `block.rs` 确认 `Voice { data }` |

### 需更正/细化的点

1. **代码路径前缀**：文档使用 `aero-server/src/` 而非 `crates/aero-server/src/`。虽然分析场景中路径仍可 grep，但按项目 AGENTS.md §4.4 约定「模块名当锚点」，实际路径前缀是 `crates/aero-server/src/`。

2. **moderation bot 位置**：文档写 `bin/boot/moderation_bot.rs`，实际是 `crates/aero-server/src/moderation_bot.rs`。不是根 `bin/boot/` 子目录。

3. **NATS 重试退避**：文档说「无退避上限或熔断」。代码实际有 1 秒恒定退避（`BUS_RESUBSCRIBE_BACKOFF`），但确实无指数退避或熔断。可修正为「恒定短退避，无指数退避/熔断」。

4. **`MESSAGES_SENT_TOTAL` 计数收口**：文档在方向二指出「服务器持久化成功但 NATS 失败→消息入库未广播」。但这个 metric 在 `bus.rs` 中**仅在 legacy 回退路径**计数，主路径的计数在 `send_message` 中已完成（`messages.rs`）。这是一个值得注意的细节：NATS 发布失败不影响计数，但**影响扇出**。

---

## 二、分析深度评估

### 强项

| 方面 | 评价 |
|------|------|
| **代码锚点精度** | 行号级精度，远超大多数架构分析的「文件级」粒度 |
| **数据流贯穿** | 每个方向都追溯了完整请求路径（客户端→WS→服务端→NATS→扇出），而非停留在单函数分析 |
| **边界情况** | 方向三的降级层次表（L0-L4）、方向一的阻尼器需求（低通滤波）、方向五的分片传输思路——都显示了对**运行时工程**的深刻理解 |
| **与既有模式对齐** | 「新 `MessageAck` 帧」与现有 `temp_id`/`pendingByTempId` 模式自然契合，无改造冲突 |

### 可进一步深挖的点

1. **方向一（PG 池保护）：`pg_read` 的潜力未充分评估**
   代码已有 `state.pg_read`（主从分离的骨架），文档提到了一次但未充分分析其作为「立即见效方案」的可行性——将 AI worker 查询、搜索、分析路由到 `pg_read` 能立即减少 30-40% 的主库连接竞争，比自适应限流的实现成本低得多。

2. **方向三（降级编排）：与 `health`/`ready` 端点的互动**
   文档提及了 readiness 探针（方向三边界情况），但未充分探讨「降级层级直接反馈到 `/ready` 端点」的设计——当系统处于 L2 降级时，负载均衡器应将流量导向其他实例。这与当前 `/ready` 的实现（仅探 PG/Redis/NATS 的连接性）形成对比。

3. **方向四（自适应限流）：与方向一的关系**
   文档的优先级排序将方向一放在 P0、方向四放在 P2，但方向四的健康聚合器**是方向一的直接前置依赖**。文档在依赖关系中承认了这一点，但排序未充分体现「方向四的方向一子集」可作为方向一的 MVP 实现路径。

4. **缺失的维度：NATS 背压**
   整个分析没有涉及 NATS JetStream 的 `max_payload`（默认 1 MiB）对系统行为的限制。方向五提到了 NATS 帧尺寸，但未延伸到：当系统过载时 NATS JetStream 的磁盘/内存使用对 PG 连接池的间接影响（NATS 慢了 → 消息路径中的 `publish_bytes` await 变长 → 占住 web handler 不放 → 连接池压力放大）。

---

## 三、工程实用性评估

| 方向 | 代码就绪度 | 实施路径清晰度 | 投产风险 |
|------|-----------|--------------|---------|
| **方向一** | 高——已有 `metrics_tasks.rs` 的池利用率 gauge，只需添加反馈回路 | 清晰 | 低——可增量实施：先 gauge-only 观察，再加限流响应 |
| **方向二** | 中——需新 `MessageAck` 帧 + 客户端处理；服务端已有 `temp_id` 就绪 | 非常清晰 | 低——新增帧，不改变现有消息流 |
| **方向三** | 低——需要新 `HealthAggregator` 模块 + 逐模块改造 | 清晰但实施量大 | 中——改造过程中可能改变现有 fail-open 行为 |
| **方向四** | 中——限流器已有 `DashMap` 接口可扩展 | 清晰 | 低——初期只添加衰减因子，不影响静态配置 |
| **方向五** | 高——纯新增校验逻辑，无架构侵入 | 非常清晰 | 极低——配置项默认不限制，可选开启 |

### 隐藏的「好挖」机会

文档的分析在以下具体问题上比表面的五方向更深，值得单独提取：

1. **`room_member_cache` 热路径缓存**：`bus.rs:95` 使用 `state.room_member_cache.get_or_fetch` 在扇出前查房成员。如果缓存未命中→PG 查询→与消息路径争连接。这在方向一的分析范围内，但文档没有显式提到这个具体的连接竞争点。

2. **`DbPool::size()` vs `num_idle()` 的非原子性**：`metrics_tasks.rs` 中 `size - idle = in_use` 的计算在并发下不是原子快照。这虽然不关键（gauge 本来就是采样），但在用作自适应限流的反馈信号时会导致瞬时误判。

3. **WS 重连时的 `manifest()` + `assert_room_access` 风暴**：文档方向一描述了「WS 重连风暴打爆连接池」的场景。这是已经可以通过 `room_member_cache` 缓解的问题，但重连时每个房间都触发 `assert_room_access`（5+ JOIN）→ PG 负载。当前缓存只覆盖了扇出路径的成员列表，没有缓存房间访问检查结果。

---

## 四、总结

| 维度 | 评级 |
|------|------|
| 事实准确性 | ★★★★☆（4.5/5——路径前缀问题和 moderation_bot 位置需修正） |
| 分析深度 | ★★★★★（5/5——行级锚点、数据流贯穿、运行时工程思维） |
| 实用价值 | ★★★★★（5/5——五个方向均可行、有清晰实施路径、与现有代码模式对齐） |
| 完整性 | ★★★★☆（4/5——NATS 背压缺失、pg_read 潜力未充分挖掘） |

这是一份 **生产级代码库的严肃架构诊断**，不是走马观花的表面分析。五个方向中，方向一、方向二、方向五都是可以**单周内实施**的高价值改进。方向三（降级编排）虽然是投入最大、影响面最广的方向，但文档提出的降级层次表（L0-L4）为后续工程提供了可直接实现的分类框架。

建议在进入实施前，补充以下两点现场观察：
1. `pg_read` 在主从部署下能直接消化多少负载（压测基线）
2. 当前 `room_member_cache` 的命中率（决定方向一中缓存优化 vs. 限流反馈的优先级）

*以上。提交日期：2026-07-12，基于源码 crate 标注验证。*
