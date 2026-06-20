# REFACTOR_PLAN.md — P0 代码拆分执行计划

> 当前 14 个 HARD 违规文件（> 1200 行）需优先拆分。
> 执行顺序按**影响面从大到小**：先拆被最多文件引用的 crate。

## 执行顺序

| 优先级 | 文件 | 行数 | 引用数 | 拆分为 |
|---|---|---|---|---|
| **P0** | `aero-im-core/src/service.rs` | 2418 | 被 4 个 crate 引用 | `service/messages.rs`, `reactions.rs`, `reads.rs`, `notifications.rs` |
| **P1** | `aero-storage/src/message.rs` | 1819 | 被 8 个 crate 引用 | `message/repo.rs`, `search.rs`, `retention.rs` |
| **P2** | `aero-storage/src/workspace.rs` | 1750 | 被 6 个 crate 引用 | `workspace/repo.rs`, `members.rs`, `settings.rs` |
| **P3** | `aero-ai/src/service.rs` | 2011 | 被 2 个 crate 引用 | `service/summarize.rs`, `answer.rs`, `embed.rs`, `moderate.rs` |
| **P4** | `aero-server/src/ws.rs` | 1378 | 仅 aero-server | `ws/handler.rs`, `rooms.rs`, `calls.rs`, `streams.rs` |
| **P5** | `web/app.js` | 2056 | 仅 web | `views/auth.js`, `room.js`, `stream.js`, `call.js`, `settings.js` |
| **P6** | `aero-server/src/bin/aero-server.rs` | 1515 | 仅 aero-server | 可暂停（启动逻辑高密度） |
| **P7** | `aero-common/src/model.rs` | 1519 | 被所有 crate 引用 | 谨慎：`model/room.rs`, `message.rs`, `block.rs` |

## Step-by-Step 执行步骤（Agent 按顺序执行）

### Step 1: `aero-im-core/src/service.rs` → `service/` 子模块

**目标**：2418 行 → `service/mod.rs`(~80行) + 4 个子文件(~600行/个)

**具体操作**：

1. 创建目录：`crates/aero-im-core/src/service/`

2. 创建 `service/messages.rs`：
```rust
// 从 service.rs 提取: send_message, edit_message, delete_message, forward_message
// 共约 600 行
use crate::*;  // 共享依赖
impl ImService {
    pub async fn send_message(&self, ...) -> Result<...> { ... }
    pub async fn edit_message(&self, ...) -> Result<...> { ... }
    pub async fn delete_message(&self, ...) -> Result<...> { ... }
    pub async fn forward_message(&self, ...) -> Result<...> { ... }
}
```

3. 创建 `service/reactions.rs`：
```rust
// 从 service.rs 提取: add_reaction, remove_reaction, list_reactions, reaction_detail
impl ImService {
    pub async fn add_reaction(&self, ...) -> Result<...> { ... }
    pub async fn remove_reaction(&self, ...) -> Result<...> { ... }
}
```

4. 创建 `service/reads.rs`：
```rust
// 从 service.rs 提取: mark_read, mark_channel_read, get_unread_count, get_read_receipts
impl ImService {
    pub async fn mark_read(&self, ...) -> Result<...> { ... }
}
```

5. 创建 `service/notifications.rs`：
```rust
// 从 service.rs 提取: send_notification, NotifyBatch 展开, push_bot 调用
impl ImService {
    pub async fn send_notification(&self, ...) -> Result<...> { ... }
}
```

6. 创建 `service/mod.rs`：
```rust
pub mod messages;
pub mod reactions;
pub mod reads;
pub mod notifications;

// 保持原 service.rs 暴露的公共 API 完全兼容
pub use messages::{send_message, edit_message, delete_message};
pub use reactions::{add_reaction, remove_reaction};
pub use reads::{mark_read, get_unread_count};
pub use notifications::send_notification;

pub struct ImService { ... }
```

7. **验证**：
```bash
cargo check --workspace --quiet
cargo test --workspace --lib --quiet
bash scripts/file-size-check.sh  # 确认 service.rs 从 2418 降到 ~80
```

**成功后**：`make check-rebase` 的 HARD 违规从 0 新增保持为 0。

**然后**：
```bash
# 更新基线（记录 service.rs 已从 2418 降到 ~80）
make init-baseline
```

这时 `scripts/file-size-check.sh` 的 HARD 违规从 14 → 13。

### Step 2: `aero-storage/src/message.rs` → `message/` 子模块

**目标**：1819 行 → `message/mod.rs`(~50行) + 3 个子文件(~600行/个)

**具体操作**：

1. 创建 `crates/aero-storage/src/message/repo.rs`：CRUD 操作（insert, get, list, update, delete_soft）
2. 创建 `crates/aero-storage/src/message/search.rs`：FTS、向量、混合搜索查询
3. 创建 `crates/aero-storage/src/message/retention.rs`：清扫逻辑（sweep_expired, sweep_ephemeral）
4. 创建 `message/mod.rs`：`pub use` 重新导出

**验证同上**。

### Step 3-7: 同理依序执行

每个 step 完成后运行 check-harness 看违规计数下降。

## 验收标准

```bash
# 重构完成后：
bash scripts/file-size-check.sh
# 输出: 0 HARD 违规, 0 WARNING

make check-full
# 输出: ✓ Full check passed — ready to commit
```

## 重构期间的检查流程

在重建期间**不要使用 `make check-harness`**（它会因为基线中的 14 个 HARD 违规而失败）。

使用 refactor 模式：

```bash
# 1. 每次修改后运行（只检查新增违规）：
make check-rebase
# 等待输出: "0 新增违规"

# 2. 每完成一个文件拆分后更新基线：
make init-baseline
# 输出: "✓ 基线已记录: 13 HARD, 17 WARN"（递减）

# 3. 所有文件拆分完成后恢复标准检查：
make check-full
```

完整的重构迭代循环：

```
读 REFACTOR_PLAN.md
↓
确定当前 Step（eg. Step 1: service.rs）
↓
执行拆分（skills/refactor-large-file.md）
↓
make check-rebase  ← 必须通过
↓
make init-baseline ← 更新基线
↓
cargo test --workspace --lib ← 必须通过
↓
开始 Step 2
```

## 重构完成后的过渡

所有 7 个 Step 完成后（0 HARD 违规），Agent 应：

```bash
# 1. 确认无违规
bash scripts/file-size-check.sh
# 输出: 0 HARD, 0 WARNING → 重构完成

# 2. 更新任务系统
# 在 CURRENT_SPRINT.md 中:
#   - 将 Phase 1 的 📊 进度改为 "14/14 已完成 ████████████ 100%"
#   - 将状态从 "🔴 REFACTOR 阶段" 改为 "🟢 功能开发阶段"
#   - 删除 Phase 1 章节（或折叠存档）
#   - 激活 Phase 2 任务看板

# 3. 启动开发模式
cargo test --workspace --lib --quiet
make check-full
bash scripts/task-planner.sh
# → 输出: "分配到: Phase 2 — 功能开发"
# → 显示 CURRENT_SPRINT.md 中第一个 [ ] 任务
```

## 回滚策略

```bash
git checkout -- crates/aero-im-core/src/service.rs  # 恢复原文件
# 重试拆分（检查是否遗漏了 pub use 导出）
```
