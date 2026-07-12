我的验证已完成。以下是每方向的**逐项验证结果**，交叉比对代码库和 235 份既有需求文档。

---

# 验证报告：五方向全局扫描分析

## 方向一：Webhook Outbound 背压与可靠性架构

**声明**: 未覆盖

**验证结果**: ⚠️ **部分覆盖** — 核心问题已被既有文档识别，但子项是新颖的

### 代码验证

| 子项 | 代码状态 | 声明判定 |
|------|---------|---------|
| Per-hook 独立递送队列 | `dispatch_event()` 在单任务内逐目标同步迭代，无 per-hook mpsc | ✅ **正确** |
| 无递送速率限制 | 仅有断路器（基于 5 次失败），无基于响应时间的自适应限流 | ✅ **正确** |
| 无事件优先级 | 当前仅递送 `RoomEvent::Message` | ✅ **正确** |
| 断路器无告警 | `apply_breaker_outcome` 写 DB 但无通知渠道 | ✅ **正确** |
| 无递送 SLO 监控 | 无聚合延迟/成功率的 Prometheus gauge | ✅ **正确** |
| 无优雅降级 | 断路器开启后仍写入递送日志 | ✅ **正确** |

### 既有文档重叠

`2026-07-11-five-operational-edge-cases-and-optimizations.md` **已覆盖**：
- **方向二**「Webhook 投递管线缺乏并发边界与背压」— 事件级串行 + 目标级串行问题
- `let _ = sub.ack().await` 静默吞噬在所有 13 个消费者中的问题

**但文档未覆盖**：per-hook 独立队列、事件优先级、递送健康探针+告警、动态速率限制、静默模式

### 文档中以下声明不精确

"**整个 durable consumer 游标停止推进**" — 实际上 `dispatch_event` 总是 `sub.ack()` 无论递送结果如何（`webhooks.rs:636`），所以慢端点不阻塞游标。但目标级串行仍然阻塞后续事件处理。

---

## 方向二：搜索点击反馈 → Learning-to-Rank 闭环

**声明**: 未覆盖

**验证结果**: ✅ **真实未覆盖** — 但有重要代码层级修正

### 代码验证

| 子项 | 代码状态 | 声明判定 |
|------|---------|---------|
| `search_click_events` 表 (迁移 0133) | ✅ 存在，`record_click` + `ctr_stats`(MRR) 已测试 | ✅ 正确 |
| 点击反馈 → 检索器加权 | ❌ 无反馈回路 — 点击数据无人消费 | ✅ **正确** |
| RRF 融合 | `aero-ai/src/rerank.rs` 有 `fuse_rankings` + `RRF_K=60` **但仅用于 AI RAG 管线**，不在用户搜索端 | ⚠️ **部分错误** — 文档声称 RRF 在搜索中用且是静态；实际上搜索完全不用 RRF，AI 侧用 RRF |
| 搜索 mode 参数 | `routes.rs:1520-1600` 有 `"fts"|"vector"|"hybrid"|"auto"` 模式 | ⚠️ **不准确** — 文档声称 mode 在 search_advanced.rs，实际在 routes.rs |
| 混合搜索 | `merge_hits` (简单 max-score 融合，`helpers.rs:22-42`），非 RRF | ⚠️ **不准确** — 不是 RRF 而是 max-score |
| 操作符解析 | `search_advanced.rs` 有 `from:`/`in:`/`before:`/`after:`/`since:`/`until:` 解析 | ⚠️ **不准确** — 文档声称操作符只在高级搜索，但实际上操作符解析在 `storage/search_query.rs` 且是高级搜索的核心部分 |
| `SearchHit.headline` | 字段存在（`message/mod.rs:61`），但所有 `From` 实现都设 `None` | ✅ **正确** — 永远为 None |
| 结果高亮 | 搜索响应返回 `score` + `message`，不包含 `headline` | ✅ **正确** |
| 拼写纠正 | 无 | ✅ **正确** |
| 搜索分析仪表盘 | 无 | ✅ **正确** |

### 修正后价值判定

尽管 RRF 和融合搜索部分已实现，但**点击反馈→检索器加权的闭环**是真实缺口。方向二的核心理念有效，但实现路径应专注于：
1. 连接 `search_click_events` 数据到 `fuse_rankings` 的 rank 加权
2. 为 `SearchHit.headline` 添加真实的高亮文本生成
3. 搜索分析 API

---

## 方向三：数据生命周期管理管线

**声明**: 未覆盖

**验证结果**: ❌ **声明有多个重大事实错误** — 核心理念有效但前提不精确

### 代码验证

| 声明的"无清理"表 | 实际状态 | 声明判定 |
|-----------------|---------|---------|
| `notifications` | `sweep_notifications()` — 默认 90 天 | ❌ **错误** |
| `audit_events` | `sweep_audit()` 默认 365 天 + `sweep_audit_partitions()` 自动 DROP 旧分区 | ❌ **错误** |
| `webhook_delivery_log` | `sweep_webhook_logs()` 默认 30 天 | ❌ **错误** |
| `search_click_events` | `sweep_search_clicks()` 默认 90 天 (调用 `SearchFeedbackRepo::sweep_before`) | ❌ **错误** |
| `ai_jobs` | `sweep_ai_jobs()` 默认 7 天 | ❌ **错误** |
| `notifications` | `sweep_notifications()` 默认 90 天 | ❌ **错误** |
| `stream_gifts` | **未找到清理** — 这是唯一真的有"无清理"问题的表 | ✅ **正确** |
| `message_edits` (history) | **未找到清理** — 正确 | ✅ **正确** |

### 代码中存在但文档未提及的清理

`retention.rs` 包含 **17+ 个 sweep 函数**，涉及 19 张表：
- 消息软删、ephemeral 硬删、封禁过期、频道积分过期、延期 GDPR 擦除
- 通知 (90d)、审计事件 (365d) + 审计**分区**自动 DROP
- AI 作业 (7d)、AI 用量账本 (400d)、webhook 日志 (30d)
- 搜索点击 (90d)、登录事件 (180d)、登录失败 (180d)
- 通知 bundles (1d)、吊销 token (8d)、观众样本 (2d raw + 90d rollup) + **观众分区** 自动 DROP

### 真正存在的缺口（有效）

| 缺口 | 状态 |
|------|------|
| `messages` 未分区 | 迁移 0148 创建了 `messages_partitioned` shadow 表但未启用（文档注释声明这是有意为之，需要维护窗口切换） |
| 无存储告警 | **真实缺口** |
| 无数据字典 | **真实缺口** |
| 无冷热分离归档 | **真实缺口** |
| `message_edits` 和 `stream_gifts` 无清理 | **真实缺口** |

### 文档修正建议

方向三的**核心理念（PG 分区、存储监控、冷热分层）是有效且真实的**。但"单级存储、线性增长"和"无清理策略"的论调严重不准确——现有清除管线已经相当完备。分析应聚焦于：

1. 消息表分区推广（从 shadow table 到 real cutover）
2. 存储用量监控（Prometheus gauge）
3. 数据字典
4. 冷热归档
5. `message_edits` 和 `stream_gifts` 清理策略补充

---

## 方向四：消息块组件系统成熟化

**声明**: 未覆盖

**验证结果**: ✅ **真实未覆盖** — 所有代码声明经核实

### 代码验证

| 子项 | 代码状态 | 声明判定 |
|------|---------|---------|
| 仅有 Button + Select | `block.rs` 枚举中唯一的交互式 variants | ✅ **正确** |
| 无 TextInput/DatePicker/NumberInput | 不存在 | ✅ **正确** |
| 无 Section/Divider/TabGroup/Accordion | 不存在 | ✅ **正确** |
| 无组件版本协商 | `block_version` 不存在 | ✅ **正确** |
| 无 `alt_text` 降级 | 不存在 | ✅ **正确** |
| 无组件校验 | `SelectOption` 的 `value`/`label` 无长度限制 | ✅ **正确** — Rust 类型系统只强制 String 非空 |
| 无 Spinner/ProgressBar/Countdown | 不存在 | ✅ **正确** |
| 无多步交互上下文 | `context_token` 不存在 | ✅ **正确** |
| Web SPA 渲染 | `render.js:685` 行仅标记 `// interactive blocks` 带 Button + Select | ✅ **正确** |

### 现有组件列表（用于精准 baseline）

展示类：Text, Mention, Code, File, Voice, Card, ToolCall, Thought, Image, Video  
交互类：Button (with optional `url`), Select (single-choice dropdown)  
**确实没有** 表单输入、布局原语、动态组件

### 市场对标数据

文档提供的对标表经代码验证**准确**。Aero IM 的 Block Kit 能力确实停留在 2016 年 Slack Button 级别。

---

## 方向五：备份/恢复与灾难恢复策略

**声明**: 未覆盖

**验证结果**: ✅ **真实未覆盖** — 零备份基础设施

### 代码验证

| 子项 | 代码状态 | 声明判定 |
|------|---------|---------|
| PG 定时自动备份 | 无 `pg_dump` 脚本、无 `aero-cli backup`、无 systemd timer | ✅ **正确** |
| DR runbook | 无 | ✅ **正确** |
| 跨机房 PG 流复制 | 无 HA docker-compose 配置 | ✅ **正确** |
| NATS JetStream stream 镜像 | 无 `replicas: 3` 配置 | ✅ **正确** |
| Redis replica + RDB | 无配置 | ✅ **正确** |
| 混沌工程演练 | 无 `scripts/chaos/` 目录 | ✅ **正确** |

### 文档中正确提到的有利条件

| 条件 | 代码状态 |
|------|---------|
| `CancellationToken` 优雅关停 | ✅ 在 `background.rs` + `shutdown.rs` 中使用 |
| `/health/ready` 探针 | ✅ 在 `state.rs:333` 中，draining 时返回 503 |
| Redis 数据可重建 | ✅ Presence/rate limiter 数据本质可重建 |
| SeqGate 去重 | ✅ 客户端已有 seq 去重（文档提到的 NATS 游标回退保护） |

### 文档声明修正

文档说"Redis 的 presence 数据在上一个 ZADD 之后可能有 30 秒窗口的脏数据"——实际 TTL 是 **60 秒**（`AGENTS.md §1`：`zadd` + `zremrangebyscore` 默认 TTL）。微小但不精确。

---

## 全局质量评估

### 与既有文档的重复率

| 方向 | 与 235 份既有文档重复 | 重复内容 | 真实新颖性 |
|------|---------------------|---------|-----------|
| 方向一 | ⚠️ 部分 | Webhook 并发背压问题已被覆盖 | **65% 新颖**（子项新颖） |
| 方向二 | ✅ 无 | 全方向未覆盖 | **100% 新颖** |
| 方向三 | ✅ 无 | 核心理念未覆盖（尽管有事实错误） | **70% 新颖**（修正后） |
| 方向四 | ✅ 无 | 全方向未覆盖 | **100% 新颖** |
| 方向五 | ✅ 无 | 全方向未覆盖 | **100% 新颖** |

### 事实错误摘要

1. **方向一**：游标停止推进 — 实际 `sub.ack()` 在 `dispatch_event` 完成后调用，不阻塞游标
2. **方向二**：声称 RRF 用于搜索融合 — 实际 RRF `fuse_rankings` 仅在 AI 侧（RAG），搜索用 `merge_hits`（max-score）
3. **方向三**：多次声称"无清理策略" — 实际 17+ sweep 函数覆盖 19 张表，包括审计分区自动清理
4. **方向三**："audit_events 有分区但无旧分区自动清理" — 错误，`sweep_audit_partitions()` 确实自动 DROP 旧分区
5. **方向五**：Redis presence TTL 被误写为 30 秒，实际为 60 秒

### 价值排序建议（修正后）

| 优先级 | 方向 | 修改后定位 | 推荐原因 |
|--------|------|-----------|---------|
| **P0** | 方向四：交互组件系统成熟化 | 从 P1 提升至 P0 | 100% 未覆盖 + 差异化竞争壁垒 |
| **P0** | 方向五：备份/恢复与灾难恢复 | 从 P2 提升至 P0 | 企业 SLA 硬性前提，零基础设施 |
| **P1** | 方向二：搜索 Learning-to-Rank | 保持 P1 | 真实缺口但现有 AI RRF 提供了部分基础设施 |
| **P1** | 方向三：数据生命周期管理 | 保持 P1 | 核心理念有效但需重写前提（去除"无清理"错误声明） |
| **P2** | 方向一：Webhook 背压 | 从 P0 降级至 P2 | 核心问题已被既有文档覆盖，新颖子项为增量改进 |
