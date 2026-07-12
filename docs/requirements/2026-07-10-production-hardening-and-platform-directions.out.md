基于对全仓的代码验证，以下是逐方向的**确认/修正**评估：

---

## 代码验证报告

### 方向一：HLS 防盗链与流保护 ✅ **完全确认**

**serve.rs:123-125** 验证通过：
```rust
.nest_service("/hls", ServeDir::new(&hls_dir))  // 在 middleware 之前
// ...
.layer(middleware);
```

HLS 路径完全绕过中间件（无 `inject_request_id`、无 `CompressionLayer`、无 CORS、无限流、无 IP 白名单）。这是真实的权限缺口——`/hls/*` 无任何身份校验，任何知道 URL 的人可播放任意直播。

**推荐 P0 修复**：将 `.nest_service("/hls")` 移到 `.layer(middleware)` 之后即可纳入基础安全防护。**~3 行改动。**

---

### 方向二：WS 传输优化 ⚠️ **部分确认，有一处过时**

**确认**：
- ✅ 全仓无 `permessage_deflate`/`WebSocketConfig` 引用——压缩完全未启用
- ✅ 无帧批量化——`fan_out_raw` 逐条扇出

**修正**：
- 文档说 `try_send` 失败后「静默丢弃」，但实际代码已有 `disconnect_on_full` + `lossy` AtomicBool + `RESYNC_FRAME` 重同步机制（`hub.rs` 第 38-43 行、第 321-350 行）。这不是「完全静默」，有慢消费者保护和恢复通道
- 但缺少：
  - ❌ per-subject 处理延迟指标（`aero_bus_process_latency_seconds` histogram 不存在）
  - ❌ 发送端背压的 Prometheus 计数
  - ❌ 批量化

**修正后结论**：缺口真实，优先级仍为 P1，但节省了慢消费者处理的工程量（已有 resync 框架）。

---

### 方向三：客户端离线韧性 ⚠️ **基本确认，需校准一处**

**确认**：
- ✅ 无 IndexedDB 消息发送队列
- ✅ 无 `navigator.onLine` / `online` / `offline` 事件处理
- ✅ `ws.send()` 方法（`web/ws.js:175-181`）返回 `false` 但 `sendMessage`（`web/ws.js:188`）不处理返回值——消息丢失无用户可见反馈
- ✅ 无乐观发送——`send_message` 后等服务器返回消息才渲染
- ✅ `localStorage` 仅存储认证令牌，无草稿自动保存

**修正**：
- 文档提到 `app.js` 的 `sendText` 和 `sending` 标志，但经查找 `sendText` 函数不存在于 `web/app.js`——搜索无匹配。`web/ws.js` 已有 `sendMessage` 方法（行 188），走 `send()`。文档参考的可能是比当前代码旧的版本
- `closedByUser`、`_lastSeen`、`_seqGate` 均已实现（`web/ws.js`）——重连后 `?since=` 游标恢复消息完整，这部分工作已就绪，**无需重做**
- 核心缺口缩小为：**发送队列 + 草稿持久化 + 乐观更新**，不是完整的重连恢复重写

---

### 方向四：消息数据生命周期管理 ⚠️ **基本确认，迁移 0148 已做一部分**

**确认**：
- ✅ `messages` 表无 `archived_at`、`storage_tier` 列（`0001_init.sql`）
- ✅ 当前 retention sweep 只软删（`deleted_at IS NOT NULL`），不回收物理 TOAST 存储
- ✅ 157 个迁移，`messages` 表只增不减

**修正**：
- 迁移 `0148_messages_partition_shadow.sql` 已经创建了 `messages_partitioned` 影子分区表 + `backfill_messages_partition()` 函数 + `ensure_messages_partitions()` 维护函数
- 文档未提及这份已存在的资产。影子表是 Step A-B 就绪状态，仅 Step C 破坏性 cutover 未执行
- 缺口需重新定位：不是「缺少分区方案」，而是 **「分区 cutover 未执行 + 归档层（warm/cold tier）未实现 + 物理 TOAST 回收未做」**

**修正后结论**：需要一份 **Step C cutover 计划** + 归档层路由 + 冷存储出口。迁移 0148 的文档引用 `docs/runbooks/messages-partitioning.md`——建议在实施前先读那份 runbook。

---

### 方向五：错误分类体系与消费者健康可观测 ⚠️ **部分确认，push unwrap 是误报**

**确认**：
- ✅ 无 per-subject bus 消费者指标——`run_bus_listener` 无 `aero_bus_process_latency_seconds` histogram、无 `aero_bus_messages_total` counter
- ✅ 无消费者健康端点（`GET /health/consumers`）
- ✅ 无 `retryable` 标记——`crates/aero-common/src/error.rs` 的 `Error` enum 有 `status_code()` 和 `code()` 方法，但无法区分"5xx 可重试"与"5xx 不可重试"
- ✅ 无 `Unavailable` / `Timeout` variant——超时/上游故障全部映射为 `Internal`(500)

**修正**：
- 文档称 `Error` 全部是 `Error::Internal(String)`——**不准确**。实际 enum 已有 9 个变体：`NotFound`、`Unauthorized`、`Forbidden`、`Conflict`、`Invalid`、`RateLimited`、`Upstream`、`Database`、`Serde`、`Internal`。已有初步分类，但缺少 `retryable` 维度
- 文档称 push bot 在 `aero-push/src/lib.rs:8-15` 有 `unwrap()` 导致 panic——**误报**。生产代码中：
  - 行 331：`resp.text().await.unwrap_or_default()` → 安全（`unwrap_or_default`）
  - 其余所有 `unwrap()` / `expect()` 都在 `#[cfg(test)]` 块内
  - 真实 push_bot 的 `classify_response` 已有 `PushError` 错误分类（Auth/Transport/Rejected）

**修正后结论**：五个子方向中，「修复 push unwrap」可移除；其余四个子方向（ErrorKind 分类 + retryable + per-subject 指标 + 消费者健康端点）仍然有效。

---

## 修正后的优先级与体量评估

| 方向 | 原始体量评估 | 修正后体量 | 原始工时 | 修正后工时 | 修正理由 |
|------|-----------|-----------|--------|-----------|---------|
| 方向一 HLS 访问控制 | S-M | **XS-S** | 2-4 天 | **~1 天** | P0 修复仅 3 行重排 + 路由重组；签名令牌鉴权可复用既有 HMAC |
| 方向二 WS 传输优化 | M | **M** | 3-5 天 | **2-4 天** | 慢消费者保护 + resync 已存在，节省背压基础工量 |
| 方向三 客户端离线韧性 | M | **S-M** | 4-6 天 | **3-5 天** | WS 重连 + `?since=` 恢复已实现，只需发队列 + 草稿 + 乐观更新 |
| 方向四 消息数据生命周期 | M | **M-L** | 5-8 天 | **4-6 天** | 迁移 0148 已完成分区影子表 + backfill 函数，Step C cutover + 归档路由 + 冷存储 |
| 方向五 错误分类与可观测 | M | **S** | 4-6 天 | **3-4 天** | 去掉 push unwrap 误报；Error enum 已有 9 变体，只需加 retryable + 消费者指标 |

---

## 基于验证的 3 条 actionable 建议

### 1. [最快收益] 方向一 P0：HLS 移入中间件栈

在 `serve.rs` 将第 123 行 `.nest_service("/hls", ServeDir::new(&hls_dir))` 移到第 125 行 `.layer(middleware)` **之后**。HLS 路径即刻获得 `inject_request_id`、限流、IP 白名单、CORS 保护。**改动<5 行，影响极大。**

### 2. [误报回收] 方向五 push unwrap 从 TODO 移除

文档称 `aero-push/src/lib.rs:8-15` 有生产 unwrap → 验证后所有 unwrap 均在 `#[cfg(test)]` 内。生产代码唯一非成功路径的 `unwrap_or_default` 是安全的。**释放约 30 行改动量的预算到真正缺口。**

### 3. [资产利用] 方向四先读 `docs/runbooks/messages-partitioning.md`

迁移 0148 已建立完整的影子分区基础设施，但文档未提及。实施方向四前必须先读 runbook（定义分区 cutover 步骤 C 的详细操作），避免在已有资产上重复造轮。**推荐执行路径**：读 runbook → 执行 Step C cutover → 实现 warm tier 归档路由 → 考虑冷存储（可选）。
