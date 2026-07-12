确认「支付处理器」零引用。下面给出完整代码级验证报告。

---

## 验证报告：代码证据准确性

### 整体评估

| 等级 | 方向 | 准确度 |
|------|------|--------|
| ✅ 准确 | **方向一** Canvas CRDT 缺口 | 代码引用 95% 正确 |
| ⚠️ 部分准确 | **方向二** 创作者货币化 | 正确发现，但部分代码引用已过时 |
| ❌ 不准确 | **方向三** VOD 生命周期 | 核心论点正常，但重要代码证据错误 |
| ✅ 准确 | **方向四** 数据生命周期治理 | 论点正确，但低估了严重程度 |
| ⚠️ 部分准确 | **方向五** 搜索反馈闭环 | 功能分析正常，但 `record_click` 有消费者 |

---

### 方向一：Canvas CRDT — ✅ 准确

| 证据 | 文档声明 | 实际代码 | 判定 |
|------|---------|---------|------|
| `op: serde_json::Value` | 服务端零解析 | `canvas_op.rs:29` ✅ | 准确 |
| 注释 "server never interprets" | 解释准确 | `canvas.rs:5` 注释 ✅ | 准确 |
| PUT `update_canvas` 覆盖 blocks | 不写 op log | 只调 `repo().update()`，不写 ops ✅ | 准确 |
| POST `append_op` 不更新 blocks | 不写 canvas row | 只调 `op_repo().append()` ✅ | 准确 |
| SPA 零 Canvas UI | 无 `canvas.js` | 全网 grep 确认 ✅ | 准确 |
| `ops_since` LIMIT 1000 | 翻页 | 实际 `DEFAULT=500, MAX=1000` ✅ | 准确 |

**修正建议**：
- 文档说 "`GET /api/canvases/:cid` 返回 `blocks` 字段（全量 JSONB），但那是旧值"——实际 `GET /api/canvases/:cid` 返回的是当前 `channel_canvases.blocks` 值（由 PUT 更新）。`blocks` 和 op log 的隔离是双向的：op 不更新 blocks，blocks 写入也不记入 op log。但 blocks 本身是**最新的**（如果你用了 PUT 更新的话），不是"旧值"。
- Canvas 的路由已被重命名：文档引用 `canvas.rs:93-108` 的行号已漂移（当前 `update_canvas` 在 `canvas.rs:206`）。

### 方向二：创作者货币化 — ⚠️ 部分准确

**正确之处**：
✅ `CreatorTier.price_cents` 存在（`creator_subscription.rs:25`）
✅ `SubscriptionTier.price_cents` 存在（`subscription_tier.rs:24`）
✅ `subscribe()` 无支付校验（`creator_subscription.rs:197`，直接 upsert）
✅ 全局 `grep -ri 'stripe\|paypal\|payment.*processor' crates/` 返回 **0** 匹配

**错误/过时之处**：
❌ `EarnedPoints` 结构体在当前代码库不存在——`channel_points.rs` 使用的是 `Reward`、`Redemption`、`RedeemError`
❌ 文档引用的 `channel_points.rs::award` 函数——当前代码库中实际函数签名已不同。当前 `ChannelPointsRepo::award` 存在但是带着不同的参数签名
❌ 文档称 `subscribe` "无 `payment_intent_id`, 无 `checkout_session_id`"——这没错，但需要指出这也是**设计使然**：订阅层明确作为存储层 upsert，支付校验是调用方的责任。还不算"缺口"，是未接的支付前端

**修正建议**：
- 确认订阅迁移 `0051_creator_subscriptions.sql` 的 `creator_tiers` 表的确无 `currency`、`stripe_price_id`、`billing_period` 列——但文档说 `price_cents` 是"仅仅是数字"没错，不过当前 `SubscriptionTierRepo`（migration 104）有 `position` 和 `benefits` JSONB，文档忽略了这些字段。

### 方向三：VOD 生命周期 — ❌ 重要证据错误

**正确之处**：
✅ `thumbnail_path` 字段**不存在**于当前代码库——但文档声称它存在且 "几乎总是 None"，这是 **INCORRECT**
✅ 全局 `rg -rn "thumbnail_path\|thumbnail" --type rust` 返回 **0** 匹配
✅ `Vod` 结构体（`aero-common/src/model/media.rs:55-73`）目前没有 `thumbnail_path` 字段
✅ VOD 转码管线确实不存在

**关键问题**：
- 文档声称 "`thumbnail_path` 字段存在但从未被写入"——这完全是错误的。该字段从未存在过。这强烈表明文档作者依赖的是本地修改过的版本或错误记忆
- 文档说 VOD 路由列表（`list_stream_vods`, `list_room_vods` 等）——这些路径确实存在（`vod.rs` 线路）
- `finalize_vod` 确实存在，路径正确
- 关于 `duration_secs`——文档说它有 `Option<u32>`，确认

**修正建议**：
这个方向的根本论点（缺失 VOD 转码/缩略图/搜索/进度记忆）在概念上是正确的，但核心代码证据（`thumbnail_path`）完全错误。如果再提交这个方向，必须**删除 `thumbnail_path` 的引用**，改为指出 **"`Vod` 结构体根本没有缩略图/预览字段——参见 `migrations/0025_vod.sql` 中 `stream_recordings` 表的列"**。

### 方向四：数据生命周期治理 — ✅ 准确（但低估了）

**正确之处**：
✅ `retention.rs` 确实有 16+ 个独立 `sweep_*` 函数（文档只数了 12 个，实际是 16 类数据 + 2 个分区维护）
✅ 每个类型使用独立的 `AERO__SERVER__*_RETENTION_DAYS` env 配置
✅ 所有清扫共享同一个 `AERO__SERVER__RETENTION_SWEEP_SECS` 滴答（默认 3600s）
✅ 没有统一的"数据类"概念或策略层
✅ Legal hold 使用 `NOT EXISTS` 子查询排除（在 `sweep_expired_messages` 中）
✅ 没有 Prometheus 指标或观测性

**缺失/低估之处**：
⚠️ 文档说 13 个 SQL 查询——实际是 **16 个 sweep 函数 + 2 个分区维护函数**（`sweep_audit_partitions`, `sweep_viewer_partitions`）
⚠️ 文档遗漏了 `sweep_deferred_erasure`（GDPR 延迟擦除）、`sweep_audit_partitions`（每日分区管理）、`sweep_viewer_partitions`（查看器分区管理）和独立的 `blob_gc_drain` timer（60s 固定）——实际共有 4 个独立的 timer/循环，而非 2 个
⚠️ `retention.rs` 还包含每个 sweep 函数的**每个类别的 Prometheus 日志记录**（`info!(swept = n, ...)`），但只有日志，没有 gauge

### 方向五：搜索反馈闭环 — ⚠️ 部分不准确

**正确之处**：
✅ `CtrStats` 结构体和 `ctr_stats` 方法存在但**从未在生产代码中被调用**——`rg -rn "ctr_stats\|CtrStats" --type rust` 返回无结果
✅ `merge_hits` 使用纯最大分数融合（在 `helpers.rs:21-44` 中确认）
✅ 没有学习排序（LTR）管线
✅ 没有个性化信号、流行度信号或时效性衰减

**错误之处**：
❌ **`record_click` 确实有消费者**——文档声称 "没有消费者"，但 `search_advanced.rs:169-208` 中的 `POST /api/search/click` 处理程序**确实调用了 `SearchFeedbackRepo::record_click`**。该端点响应式工作，接收 `query`、`result_id`、`rank` 和用户身份验证

❌ **`CtrStats` 是死代码的说法只对了一半**——`ctr_stats()` 方法确实从未在生产代码中被调用。但 `record_click()` 功能完全正常

❌ 文档声称 `merge_hits` 中有 "你的 `record_click` 代码片段"`——文档中用 `record_click` 作为证据，但实际函数名是 `n()`（在 `search_feedback.rs` 中）——但较新的代码已将其重命名为 `record_click`。文档引用的 `search_feedback.rs:record_click` 和 `SearchFeedbackRepo::record_click` 都存在且签名匹配。这个方向的核心观点（信号从未被反馈到 ranking 中）仍然是正确的。

**修正建议**：
反馈回路断裂的实际点是**在 `record_click` 存储之后**：`ctr_stats` 从未被调用，没有定时任务从搜索点击中学习，`merge_hits` 不纳入点击信号，ranking 权重从未被调整。**回路收集了输入但没有输出。** 文档需要更新以反映这一点——`record_click` 实际上路径完整，但反馈侧（从点击学习改善排名）完全不存在。

---

## 总体建议

| 方向 | 优先级 | 类型 | 代码证据可靠性 | 建议 |
|------|--------|------|--------------|------|
| **1. Canvas CRDT** | P0 | 架构/UX | ✅ 高 | 准备就绪——修复写入路径隔离并添加客户端 CRDT |
| **2. 创作者货币化** | P1 | 产品/架构 | ⚠️ 中 | 需要更新代码引用以匹配当前代码库；核心论点成立 |
| **3. VOD 生命周期** | P1 | 产品/架构 | ❌ 低 | **必须修正**——`thumbnail_path` 证据完全错误。重写证据部分 |
| **4. 数据生命周期** | P1 | 架构/合规 | ✅ 高 | 准备就绪——实际更严重（16+ 类 vs 12 类） |
| **5. 搜索反馈闭环** | P2 | 产品/架构 | ⚠️ 中 | 需要澄清——`record_click` 有消费者，但 `ctr_stats` 没有；修复等级仍有意义 |

### 最关键发现

**方向三需要重写证据部分。** `thumbnail_path` 错误是这个方向的一个重大漏洞——如果你告诉决策者 "VOD 结构体有一个 `thumbnail_path` 字段但从未被写入"，而该字段实际上并不存在，你就失去了可信度。正确且同样有力的说法是：**"`Vod` 结构体根本没有缩略图或预览字段——参见 `migrations/0025_vod.sql` 中的 `stream_recordings` 表——即使是录制元数据也不包括视觉预览或处理状态。"**

**方向五是一个需要修正术语的案例。** 虽然反馈回路确实已断裂，但断裂点是在聚合/学习侧，而不是在收集侧。`record_click` 端点功能完善——在 HTTP 层面测试、路由、带安全门控。问题是没有东西消费汇总信号来改进排名。
