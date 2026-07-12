Now I have all the verification I need. Here is my thorough review of the document.

---

## 审阅反馈：第 33 轮全局扫描 — 5 个未覆盖方向

**已保存至**: `docs/requirements/2026-07-11-round-33-global-scan-five-uncovered-architecture-expansion-directions.md`

### 交叉验证结论

我已经对照代码库逐方向验证了所有事实性断言，并通读了 Rounds 30-32 确认无内容重叠。以下是我的架构级审阅。

---

### ✅ 方向一：嵌入式客户面对话部件

**对于既有组件的复用评估基本准确**——特别是 `hub.rs` / NATS infrastructure 和 `guests.rs` 的复用潜力确实存在。

**三个未被充分讨论的架构约束**：

1. **认证模型的根基性改变**：当前所有 WebSocket 连接都经过 `AuthUser` extractor（JWT → participant）。访客对话需要一个完全独立的认证流（短期匿名 token），且必须绕过当前整个中间件栈（该栈假设已认证用户）。这意味着**不能简单地"扩展现有路由"**——需要新建一套完全不共享当前中间件的嵌入式 API + WS 入口。这个成本在分析中被低估。

2. **NATS subject 隔离风险**：分析正确提出 `chat:{conversation_id}` 新 subject，但没有讨论隔离策略。如果和 `im.room.*` 共享同一个 durable consumer，一条访客消息的误投可能泄露给内部聊天频道。建议：`chat.*` 用**独立的 stream + consumer**（与 `im.room.*` 完全隔离），并在 `hub.rs` 中加 `FanOutStrategy::Chat` 枚举来隔离扇出路径。

3. **访客消息需要实时内容过滤（非 async AI moderation）**：当前 `moderation_bot` 是有预算约束的异步审核（非投递门）。客服场景中，访客第一条消息必须实时过滤（SQLi/XSS/spam 等）。需要同步 `visitor_message_filter` 层，这不复用现有异步 moderation 管线。

**体量建议**：L → **XXL**。这是一个新产品线，估算 2-3 个月工程 + 安全审计。

---

### ✅ 方向二：消息发送撤回与发送确认

**三种模式的分类清晰，Undo Send 的 Redis sorted-set 方案是正确的技术选型**。

**两点深层问题**：

1. **Undo Send 与 `bus/seq.rs` 的 seq 分配冲突**：当前 `RoomEvent` 在 `publish_room_event` 时分配 monotonic seq。但如果消息在 undo 窗口内尚未确认交付，它不应该被分配 seq——否则接收端看到 seq gap 或 "pending" 态消息分配了 seq 但后来又被收回。需要：延迟 seq 分配，直到 undo 窗口过期 + `confirm_delivery` 触发。这意味着 `bus::SeqMinter` 需要支持 `reserve`（预占 seq 但不提交）和 `commit(seq)` 两个阶段——这需要改 bus crate。

2. **NATS at-least-once 语义与 undo 状态机的冲突**：如果消息在 undo "pending" 态时 server 崩溃重启，durable consumer 会重新投递未确认的消息——这意味着 undo 窗口期的消息可能**在崩溃恢复后被确认发送**，即使用户本打算撤回。解决：undo 窗口的状态必须存储在 Redis（而不是内存），起服时恢复所有活跃 undo 窗口，追回崩溃期间过期的窗口。

**Send Review 与既有 `info_barriers.rs` / `approvals.rs` 的关系分析准确**——确实是不同的审批语义（消息级前置门 vs 请求级审批流）。

**体量建议**：M → **L**（因为 seq 分配和崩溃恢复两个深层问题把 Undo Send 从 M 推到了 L）。

---

### ✅ 方向三：白标与品牌定制化

**现状评估完全准确**——零品牌定制代码（已 grep 确认）。

**Phase D（自定义域名）的体量标记为 L，实际上应为 XXL**：

- ACME 客户端集成（`acme-lib` / `rustls-acme`）需新 crate 依赖，且需验证与 root `unsafe_code = forbid` 约束的兼容性
- 需要 SNI 感知的 TLS 终止（嵌入 axum 或前置 nginx 代理）
- DNS CNAME 验证 + 证书续期监控 + 失败告警
- 每租户 `certificate_expires_at` 追踪

这几乎是新建一个 Let's Encrypt 自动化平台。建议：Phase D 推迟到 Phase A/B/C 之后单独规划。

**一个缺失的分析点**：品牌配置的缓存策略。分析提到 "中间件层缓存（Cache<K=V>，短期 TTL 30s）"，但没有讨论 `workspace_id` 如何在每一次 HTTP 请求中被解析。对于非自定义域名（默认 `aero.im/workspace-slug` 路径），workspace_id 来自 URL path 参数——但路径解析在品牌渲染时已经太晚（定制 CSS 变量需要在 `<head>` 中注入）。对于自定义域名，需要一个 `Host` → `workspace_id` 的查询缓存。这可能需要一个新的中间件 `middleware/tenant_context.rs` 在路由匹配**之前**执行。

**体量建议**：总体 M 是正确的（Phase A/B/E 是 S，Phase C/F 是 M，Phase D 单独算 XXL）。

---

### ✅ 方向四：数据库迁移回滚与 Schema 生命周期管理

**最务实的方向。技术分析扎实，但我发现一个被忽略的架构约束**：

**`sqlx::migrate!("../../migrations")` 编译时嵌入 `down.sql` 的冲突**：`sqlx::migrate!` 会编译 `migrations/` 目录下的**所有 `.sql` 文件**并按文件名前缀排序执行。如果直接把 `0152_down.sql` 放在同一目录，sqlx 会将其解读为一个新的前向迁移（因为它以数字前缀开头且在 `_sqlx_migrations` 表中无记录），然后尝试执行它——这会导致 "无法删除不存在的列" 等错误。

**解决方案（分析中未提及）**：
- 选项 A：`migrations/*.up.sql` + `migrations/*.down.sql` 分开两个子目录，down.sql 用 `include_str!` 手动嵌入，在 `aero-cli` 中实现独立的回滚执行器（不通过 `sqlx::migrate!`）。
- 选项 B：`migrations/` 中用 `0152__add_foo.up.sql` / `0152__add_foo.down.sql` 文件后缀命名，让 `sqlx::migrate!` 只匹配 `.up.sql`（需检查 sqlx 是否支持 glob 模式；当前不支持——sqlx 用 `glob("*.sql")`）。
- **推荐**：选项 A（`migrations/forward/` + `migrations/rollback/`），因为 sqlx 的 migration runner 不支持文件后缀过滤，只有拆分目录才能干净分离。

**L5（Schema 版本兼容性声明）是最被低估的高价值改进**——如果标准化，可以完全消除 "deploy new binary → runtime crash because schema not migrated" 类事故。建议将其优先级从隐性的 S 体量提到 L1 并列（启动时检查，失败则 panic 输出错误信息）。

**体量建议**：S-M 正确。

---

### ✅ 方向五：管理操作审计日志

**设计最成熟的方向。`audit_admin_actions` 表 schema 设计合理，`emit_admin_audit` 函数模式正确。**

**三点补强**：

1. **`tokio::spawn` 异步写有静默丢事件的风险**：如果 spawn 的任务 panic（极端情况下——DB 连接池耗尽等），审计事件静默丢失。建议：用 `tokio::sync::mpsc` bounded channel + 专用 consumer 任务（复用 Hub 的 bounded mpsc 模式）。这样写路径永不被阻塞，consumer 失败有 panic/restart 兜底，channel overflow 时有 backpressure 信号。

2. **审计表的 append-only 需要 DB 级保护**：应用层 `no DELETE / UPDATE` 是不够的——一个 SQL injection 或 DB 账号泄露就可以修改。考虑 PG 层面的 `REVOKE UPDATE, DELETE ON audit_admin_actions FROM app_user;` 或行级安全策略（RLS）。这在提议的 migration `0158` 中就应该声明。

3. **审计风暴（SCIM sync 1000 users）的 batch 模式**：分析正确识别了这个边界情况。但实现细节值得提前明确——batch 事件同样需要不可变性。建议用 `audit_admin_action_batches` 子表 + `action='scim.sync'` 的 `details` JSONB 中保存 `user_count` 和 `affected_user_ids[]`。这样即使 batch 也能 audit trail 到具体受影响的用户。

**体量建议**：M 正确。这是最容易先做的方向（独立于其他方向，没有依赖冲突）。

---

### 跨方向评价

| 维度 | 评价 |
|------|------|
| **最大的架构洞察** | 方向一（嵌入式客服）——确实看到了另一个产品可以从同一个代码基生长出来，这是架构红利视角 |
| **最高 ROI（短期）** | 方向四（迁移回滚 L1-L2）+ 方向五（Admin Audit）——两者都可在 1-2 周内完成，且直接提升生产安全性和合规就绪度 |
| **最大的体量误判** | 方向一 L → 实际 XXL（需要完整的新认证模型 + 安全审计 + JS SDK）；方向三 Phase D L → 实际 XXL（ACME 自动化平台） |
| **最被低估的方向** | 方向四 L5（Schema 版本兼容性声明）——在 157 次迁移后，这是一个仅需 ~50 行 Rust + 一个启动时检查就能消除整个类别的生产事故 |
| **深层技术债务暴露** | `sqlx::migrate!("../../migrations")` 的编译时嵌入正在成为回滚能力的架构约束。这可能是考虑 `sqlx::migrate::Migrator::new(include_str!` 或独立迁移工具的信号 |

### 建议的优先级重排序

```
P1（生产安全）:
  - 方向四 L5（Schema 版本声明）—— 1 天
  - 方向五（Admin Audit）—— 1-2 周
  - 方向四 L1-L2（回滚基础设施 + CI 护栏）—— 1 周

P2（销售就绪度）:
  - 方向三 Phase A-C（品牌配置表 + CSS 变量 + 邮件模板）—— 1 周
  - 方向二 Undo Send（纯 UX 改进）—— 2-3 周

P3（产品扩展）:
  - 方向一（嵌入式客服）—— 2-3 月，独立产品线
  - 方向三 Phase D（自定义域名）—— 独立项目
```

总结：这是一份高质量的全局扫描。方向选择没有重叠，技术分析准确，边界情况识别到位。主要的审阅意见集中在体量评估偏差（方向一/三的某些 Phase）和一个关键的架构约束（sqlx compile-time embedding 与 down.sql 的冲突）。
