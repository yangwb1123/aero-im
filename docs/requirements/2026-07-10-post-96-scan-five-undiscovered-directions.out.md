现在我已经完成了对文件中每个声明的独立验证。以下是我的评估。

---

## 深入响应：第 97 轮扫描

在详细阅读后——并交叉验证了代码库——我认为这是一份**广泛且具有战略眼光的路线图缺口分析**，但在关于代码接线状态的具体事实上，存在一些需要修正的严重不准确之处。文档的判断力是健全的；只是需要事实核查。

---

### ✅ 强有力的发现（完全验证）

以下是五个方向中分析准确的方面：

**方向一（基础设施成熟度）——有洞察力且准确**
- ✅ 157 次迁移——0 个 `down.sql`。验证通过。
- ✅ 无备份/恢复脚本。验证通过：`scripts/` 中有 `truth-check.sh`、`file-size-check.sh`、`web-check.sh`，无 `backup.sh`。
- ✅ 无灾备演练 Runbook。验证通过：`docs/runbooks/` 不存在。
- ✅ `ALTER TABLE` 在百万级表上存在锁表风险——这是正确的（无安全迁移指南）。
- `_sqlx_migrations checksum` 冲突风险的分析是正确的。

**方向二（成本透明度）——有效点，但不准确**
- ✅ `ai_usage_ledger` 是唯一存在的计量表。验证通过。
- ✅ 无 per-workspace 存储/带宽/API 计量 API。验证通过：无 `GET /api/workspaces/:id/usage`。
- ✅ `blobs` 有 `byte_size` 列，但无聚合路由。验证通过。

**方向三（内容治理）——概念有效，但接线地图过时**
- ❌ **不准确**：用户声称 `ban_appeals`、`user_reports` 和 `message_reports` 的仓储层路由/处理器未接线。实际上，**它们完全被接线了**：`crates/aero-server/src/routes/routes.rs` 在第 458、478、484 行通过 `.merge(crate::{module}::routes())` 导入了它们，每个模块都有完整的处理器（`POST/GET/PATCH`）。
- ❌ **不准确**：`ban_appeals` 标注为「零接线」——实际上它的仓储和路由完整，甚至在 2025 年 6 月 17 日有完整的文件头文档。
- ⚠️ **Web UI 缺失**是正确的——没有举报/申诉的 DOM 元素。但 API 全线可用。
- ⚠️ **ban_appeals** USP 缺口是正确的——但方向是**Web UI 缺失**，而非「无路由」。

**方向四（用户激活）——准确且有影响力**
- ✅ 零 onboarding 体验。验证通过：web 端无 `onboard`/`welcome`/`guided`/`tour`/`template`/`sample`/`getting-started`。
- ✅ 无竞品导入（Slack/Teams/Discord）。验证通过。
- ✅ 无预设工作区模板。验证通过。
- 欢迎 Bot 和 onboarding checklist 的复用分析是合理且有价值的。

**方向五（孤立模块审计）——概念强，但接线表有缺陷**
- ❌ 用户表格中的每个模块（`ban_appeals`、`user_reports`、`message_reports`、`channel_retention`、`scheduled_streams`）**都有路由和处理器**。验证通过。
- ❌ `delivery_cursors` 有独立的 `DeliveryCursorRepo`（完整文件：`crates/aero-storage/src/delivery_cursor.rs`，包含 `advance`、`get`、`cursors_for`、`clear_room`），**完全在服务器接线**：`state.n` 字段（`state_builder.rs`）、`ClientFrame::DeliveryAck` 处理器（`frame.rs` 第 93 行）、以及重连时在 `ws_impl/mod.rs` 中调用 `cursors_for`。
- ❌ `scheduled_streams` 确实有 `POST`（创建）处理器——文档中包含 `post(create_scheduled_stream)`。
- ⚠️ **无 Web UI** 是这些模块的共同缺口——REST API 已就绪，但 SPA 未消耗。这是一个 **SPA 接线** 问题，而非 **服务器接线** 问题。

---

### 需要修正的事实错误

1. **孤立模块表（方向五）**：你需要重新审查接线状态。我验证了每个模块：
   - `ban_appeals` → `routes.rs:478`，完整处理器
   - `user_reports` → `routes.rs:458`，完整处理器
   - `message_reports` → `routes.rs:484`，完整处理器
   - `channel_retention` → `routes.rs:339`，完整处理器（GET + PATCH）
   - `scheduled_streams` → `routes.rs:216`，完整处理器（POST + GET + DELETE）
   - `delivery_cursors` → 独立的 `DeliveryCursorRepo`，服务器端 `state.n`，框架中 `DeliveryAck`，`mod.rs` 中 `cursors_for`

   真正的缺口是：**SPA 没有消耗这些 API**。没有举报按钮，没有申诉 UI，没有频道保留设置面板。这是个 `web/` 接线问题，而非 `rust/` 接线问题。

2. **方向三的举报/申诉 API 可用性**：方向三的核心论点——「用户无法举报消息」——是不正确的。`POST /api/rooms/:id/messages/:mid/report` 和 `POST /api/users/:id/report` 都存在且有完整的处理器。问题在于**没有 Web UI 按钮**来触发它们。

3. **`rg` 搜索为何遗漏**：你的 `rg` 命令（例如 `rg "scheduled_stream" routes.rs`）可能在 `routes.rs`（一个目录文件）上失败，而不是在 `routes/routes.rs` 上。另外，`delivery_cursors` 代码库最近将字段名重构为 `n`（短名称），这解释了为何 `rg "delivery.cursor"` 没命中——`state.n.advance(...)` 没有匹配项。

---

### 对优先级排序的建设性评估

**关于方向一的 P0 判断是正确的**。无备份、无回滚、无 DR——这在生产级系统中是站不住脚的。我建议提升 **数据备份** 至明确的 P0（无备份 = 数据丢失风险），将 Zero-Downtime Migration 指南保留为 P1。

**方向五在 P0 是错误的**——被列为孤立模块的模块实际上**已完全接线**。真正的 P0 工作是 SPA UI 接线（举报按钮、申诉表单、保留设置面板）。我会将方向五重新校正为「Web UI 治理缺口」，作为方向三的前置条件。

**方向四（用户激活）是 P1**，但我认为在商业上比方向三（内容治理）在**吸引新收入**方面更重要。Slack/Teams 导入是最高杠杆的获客功能。我会将其定为 P1，但标记为「高收入影响」。

方向二（成本透明度）的 P1-P2 排序合理。

---

### 文档中缺失的内容（补充观察）

在已审查的 96 份文档之上，还有几个视角你本可以捕捉到：

| 新断面 | 为何被遗漏 | 值得写吗？ |
|---------|-----------|-----------|
| **WebSocket 弹性**（重新连接恢复、速率受限的吞噬、从重连风暴中恢复） | 作为「生产韧性」的子话题，可能被归类 | 也许值得一个简短的注释——`ws_impl` 中有一个 `CancellationToken` 驱动的优雅关闭，但在大规模重连风暴期间没有任何客户端退避。 |
| **迁移测试覆盖率**（db_tests `#[ignore]` 门控——测试了多少次迁移？） | 低于典型架构分析的范围 | 低——不过 CI/CD 主题可能值得注意。 |
| **API 弃用生命周期**（v2 路由的治理，重命名时的向后兼容性） | 典型在 V1 之前被忽视 | 对于早期阶段来说范围过大 |
| **NATS JetStream 存储考虑**（`file` vs `memory` 存储，流复制） | 纳秒级运营关注点 | 对于运营团队的 P2 参考 |

---

### 最终结论

这份文档的价值在于**方向性战略思维**——五个方向在架构层面都是正确的，有意义的，且未被既有文献覆盖。这是值得保留的视角。

然而，接线矩阵中的事实错误削弱了方向三和方向五的整体可信度——特别是那些在文档中声称「完全独立」但实际拥有完整后端接线的模块。未来的版本应在将模块标记为「孤立」之前，对 `routes.rs` 和 `lib.rs` 进行自动化的 `grep` 检查。

如果你希望，我可以帮你：
1. 生成一份**修正后的版本**，保留战略论点但纠正接线表
2. 为五个方向中的任何一个**编写一份 P0 接线计划**（从 Web UI 举报按钮开始，因为这是真正的缺口）
3. 共同起草**方向一（备份/恢复）** 的 `scripts/backup.sh` 实现作为概念验证
