好，我已经对照实际代码库交叉验证了文档中的各项主张。以下是验证结果：

---

## 验证结果：5/8 核心主张 准确，3 项需修正

### ✅ 确认准确

| 主张 | 证据 |
|------|------|
| **文件内容搜索空缺** | `FileIndexRepo`（`file_index.rs`）只索引元数据（name/kind/size），`blobs` 表无 `text_content` 列，`BlobStore` 是纯字节存取 |
| **Admin Console 完全空白** | `audit.rs`（`AuditRepo`）、`sessions.rs`、`webhook_admin.rs`、`legal_holds.rs`、`channel_retention.rs`、`info_barriers.rs`、`twofa.rs`、`deactivation.rs`、`org_chart.rs`、`channel_roles.rs`、`workspace_security.rs`、`announcements.rs`——全部存在零 UI |
| **通知 UX MVP 级别** | `notifications.js` 确为 141 行，功能 = badge + 倒序列表 + all-read。`notification_bundles` 表（migration 0143）与 `importance_score`（migration 0139 实际列名 `n`）后端均已上线但前端未消费 |
| **`prompt()` 编辑** | `app.js:779` 确为 `const next = prompt('编辑消息:', text)`——描述准确 |
| **`commands.rs` 存在** | `/crates/aero-server/src/commands.rs`（529行）含 `parse_command` / `render_command` / `/remind` → `ScheduledRepo` |

### ⚠️ 需修正的细节

**1. `fan_out_raw` 描述（方向五）——实际实现比文档描述的更先进**

文档称 hub.rs 遍历 `room.watchers: Vec<WsSender>` 做 O(N) 扇出。但实际代码已经：

```rust
// hub.rs:311
pub fn fan_out_raw(&self, recipients: &[ParticipantId], text: &str)
```

不是房间级 watcher 遍历，而是 **per-participant 连接索引**（`self.conns: DashMap<ParticipantId, Vec<WsSender>>`）。已有：
- **`lossy` flag**：慢消费者丢帧但标记 `lossy=true`
- **Resync 帧**：队列恢复容量时发一个 `RESYNC_FRAME` 让客户端 REST 回补
- **并行扇出**：注释说明 >100 接收者时走 thread-pool 并行化
- **双模式**：`disconnect_on_full`（踢掉慢连接）或 `drop-only`（保留连接但丢帧）

方向五的 Phase A/B/C 优化中，Phase A（分桶 watcher）与 Phase B（慢隔离）**已大部分实现**。剩余未覆盖的是：按 `mode: "active" | "lurker"` 选择性扇出（Typing/Read 跳过 lurkers）和 `broadcast::Sender` 代替 `mpsc::Sender`。

**建议**：方向五优先级降为 **P3**，把描述从「搭建」改为「增量优化已搭建管线」。

**2. 迁移编号偏移**

| 文档所指 | 实际迁移 |
|---------|---------|
| "0149 迁移：aggregate_reply" | `0138_aggregate_notification.sql` |
| "0143 notification_bundles" | ✅ 正确（`0143_notification_bundles.sql`） |
| "0152 importance_score" | `0139_notification_importance.sql` |

**3. `importance_score` 列名**

DB 列是 `n REAL`（migration 0139），Rust struct 中映射为 `importance_score: f32`。这本身不矛盾，但文档可澄清 DB 列名与字段名的差异。

---

## 对整体分析的评估

**力度（整体优秀）**：这是一份极少见的严谨架构文档。与 70+ 既有分析不重复、基于源码 grep 可验证、Edge Cases 表格完整、风险识别到位。在开源/商业 IM 领域，这种级别的系统性断层分析非常有价值。

**纠正后三条建议优先级重排**：

```
原排序：    建议修正排序：
P1 文件搜索  P1 文件搜索（不变，知识管理核心差异化）
P1 Admin    P1 通知 UX（提上来——后端全就位，前端 3-5 天，日均交互频率最高）
P1 通知 UX  P1 Admin Console（有企业 waiting 时启动，否则 P2）
P2 编辑器   P2 编辑器（用户反馈驱动）
P2 扇出     P3 扇出增量优化（降级——基础架构已就位，大频道场景触发再投）
```

**核心建议**：在开始任何方向之前，先把 `AGENTS.md` §4.5 Read 的 RESYNC_FRAME 机制补齐为文档——这是方向五已建未文档化的典型案例。

是否需要我深入某个方向做更细粒度的源码级依赖分析，或者帮助起草 Phase A 的增量交付计划？
