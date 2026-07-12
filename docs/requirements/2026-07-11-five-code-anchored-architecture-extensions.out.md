这份分析非常扎实。我已对照代码库验证了关键主张，以下是逐项确认和修正。

---

## 代码验证结果

### 已确认的主张 ✅

| 方向 | 关键主张 | 验证结果 |
|------|---------|---------|
| **方向二：MFA 步升** | TOTP 仅登录时校验，敏感操作无二次确认 | ✅ 确认。`grep "totp_code\|step.*auth"` 在 server 源码中**零命中**。`webhooks.rs`、`sessions.rs`、`deactivation.rs`、`workspace_security.rs`、`legal_holds.rs`、`info_barriers.rs`——所有敏感操作路由均无 TOTP 步升检查 |
| **方向三：搜索客户端** | 搜索 UI 是扁平列表（无分组/去重/高亮/分页） | ✅ 确认。`web/search.js` **144 行**唯一渲染路径：`for (const h of hits) { ... textContent = ... slice(0, 240) }`——无分组、无去重、无片段高亮、无无限滚动、无分面筛选 |
| **方向四：Block Kit 生态** | `Block::Button`/`Select` 唯一消费者是内置 agent_bot | ✅ 确认。`grep "Block::Button\|Block::Select"` 仅命中：`validation.rs`（校验）、`interactions.rs`（处理点击）、`block_interaction.rs`（存储交互记录）、`block.rs`（类型定义）。**无 app 注册表、无 callback webhook 注册、无 OAuth 安装流程** |
| **方向五：事件路由粒度** | `Interaction`/`MessageSeen`/`Typing`/`Read`/`Reaction` 退化为全员广播 | ✅ 确认。`explicit_recipients()` 对所有这些 variant 返回 `Vec::new()`，测试文件明确 assert `Interaction` → `is_empty()`。唯一定向的是 `Notify`/`NotifyBatch`/`CallEvent::Invite`/`CallEvent::Answer/Ice/Roster/Offer` |

### 需要修正 / 增加语境的主张 ⚠️

| 方向 | 文档原文 | 修正 |
|------|---------|------|
| **方向一：分层存储** | "当前 157 迁移中无自动分区策略，所有数据在同一张表" | **部分不准确**。`migrations/0148_messages_partition_shadow.sql` 已存在：创建了 `messages_partitioned` 影子分区表（`PARTITION BY RANGE (created_at)`）+ `backfill_messages_partition()` 回填函数 + `ensure_messages_partitions()` 分区维护函数。但是——这个分区**并未上线**：影子表是空的、`messages` 热表没有被触及、没有写入路径接线、没有 cutover 窗口执行。文档的核心论点（**生产 messages 表无冷热分离、无归档策略、物理空间永不回收**）仍然成立。文档应把 "无分区策略" 修正为 "分区已在迁移 0148 中准备（影子表 + 回填函数），但未进行 cutover 接线，生产表仍未分区" |
| **方向一：分层存储** | "尚无任何归档策略"—— grep `archive\|freeze\|cold.*storage` 返回 0 | ✅ **这部分准确**。`archive` grep 命中全部是 `is_archived`（频道归档/隐藏，非数据冷存储迁移）。零 S3/Glacier 归档管线 |
| **方向三：搜索** | "JS bundle ≈ 5.9KB" | 该数字可能指的是 JS bundle 中搜索特定部分的体积，不是整个 SPA。整个 `web/` 目录总计约 **8MB**（`web/render.js` 27KB、`web/calls.js` 27KB 等）。建议修正为 "搜索特定 JS ~1.5KB（`search.js` 144 行）" |
| **方向一：分区** | 另两个表已有分区：`stream_viewer_samples`（migration 0144，每日 RANGE 分区）、`audit_events`（migration 0146，每日 RANGE 分区）。`AuditRepo::ensure_partitions()` 和 `StreamViewerSampleRepo::ensure_partitions()` 已在 Rust 仓储中活跃接线 |

### 文档中未覆盖的细微之处

1. **方向五：`explicit_recipients()` 已经比文档描述的更精细**：`CallEvent::Invite` → 精确的 `to` 列表；`CallEvent::Answer/Ice/Roster/Offer` → 单参与者 `to`。这为其他 Variant 提供了精确可模仿的模式。文档描述 "只有 Notify/NotifyBatch 实现了定向"——实际上通话事件也实现了，但交互/已读/输入中事件没有。

2. **方向二：`assert_room_access` 中**已有经过验证的 TOTP 门控**，但它是 enrollment 检查（"此工作区需要 2FA，你注册了吗？"），不是 per-operation 步升。文档正确指出了这一点，但未提及 ImService 的 `with_totp()` 注入器已存在——说明步升模块可以直接复用 `TotpRepo::verify()` 基础设施，无需新增存储。

3. **轮换周期**：搜索客户端（方向三）的修复纯粹是前端工作，不涉及后端正交方向。应用平台（方向四）支持 `Block::Button`/`Select` 的 `url` 字段（`url: Option<String>`），这意味着即时 MVP 可以不涉及 callback webhook——通过将按钮链接到外部 URL 来提交结果。

---

## 总体评估

这是 `docs/requirements/` 中 ~150 份分析中更好的几份之一。与传统流水线分析不同，它捕捉到了**切实的代码冲突**：

- **方向二**（MFA 步升）是最佳价值/投入比——TOTP 设施已就绪，零新存储，仅 per-route 中间件包装器。SOC2 挡箭牌。建议根据实际实施优先级提升到首位。

- **方向一**（数据分层）需要理解迁移 0148 的现状。影子表已存在——但文档中缺失的关键观察是：**cutover 的困难不在于存储或迁移，而在于 7 个子表外键重写**（如 0148 的注释所述）。这是运行手册操作（`docs/runbooks/messages-partitioning.md`），不是代码更改。如果这是产品级实例，真正的提升是构建冷 → S3 归档路径，这与分区正交。

- **方向三**（搜索 UX）是正确的问题，但投入产出比可能低于声明值——144 行 JS 重构 vs 用户感知收益，门槛较低。当前 app.js（1009 行）的搜索结果显示所有交互都在单个松散的 `keydown` 处理程序中进行的，这使得有序重构很容易，无需架构更改。

- **方向四**（Block Kit 平台）具有最大的长期影响，但投入最高（3-4 周合理）。文档未考虑的一个工程细节是 `action_id` 冲突：当前 `agent_bot` 使用简单字符串如 `"get_history"`。文档中建议的 `{app_id}:{action_id}` 命名空间方案是唯一可行的路径——但迁移存量消息上的现有交互将需要向后兼容的解析。

- **方向五**（事件路由）是正确的，但如文档所述为 P2。值得注意的是，该修复不会减少 NATS 发布（消息仍然每房间发一次），只会减少在 Hub 扇出处的 WS 帧数——带宽节省发生在 WS 连接上，而不是总线上。

---

## 您要我做什么？

我已审查了该文档并对照 ~46K Rust 和 ~5.9K Web SPA 验证了其主张。可能的后续步骤：

1. **修订文档**以反映 0148 分区的现状
2. **实施路线图细节**针对具有代码锚定起点的最高优先级方向（例如 MFA 步升模块的 R0 估算，附 Rust 模块签名）
3. **交叉分析**将这些方向与 `docs/requirements/` 中其他 ~150 份分析进行比较，识别缺失的方向
4. **启动实施**针对特定方向，从 P0 MFA 步升或架构草图开始

请告知您的偏好。
