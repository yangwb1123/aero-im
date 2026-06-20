# Skill: Refactor Large Rust File

**用途**：将 > 800 行的 Rust 源文件拆分为合适尺寸的模块。

**触发条件**：
- `scripts/file-size-check.sh` 输出当前文件超 HARD_LIMIT 或 WARNING
- 新增功能时发现目标文件即将超过 800 行

---

## 步骤

### 1. 识别职责

给定一个大型 Rust 文件，识别其中包含的独立职责。以 Aero IM 的 crate 模式为参照：

| 文件 | 典型拆分维度和目标模块 |
|---|---|
| `aero-im-core/src/service.rs` | `messages.rs`(发送/编辑/删除), `reactions.rs`, `reads.rs`(已读/游标), `notifications.rs`, `history.rs` |
| `aero-storage/src/message.rs` | `message_repo.rs`(CRUD), `search_query.rs`(FTS/向量/混合), `retention.rs`(清扫) |
| `aero-ai/src/service.rs` | `summarize.rs`, `answer.rs`, `embed.rs`, `moderate.rs` |
| `aero-server/src/routes.rs` | 不拆文件，但提取 handler 到 `routes_auth.rs`、`routes_rooms.rs`、`routes_streams.rs` 等 |
| `web/app.js` | `auth.js`, `room-view.js`, `stream-view.js`, `call-view.js`, `settings.js` |

### 2. 提取子模块（Rust 模式）

对目标文件 `src/large.rs`：

**a) 创建 `src/large/` 目录**

```
src/large/
├── mod.rs      ← pub mod 声明 + pub use 重新导出
├── part_a.rs   ← 提取的第一部分
├── part_b.rs   ← 提取的第二部分
└── part_c.rs   ← 提取的第三部分
```

**b) 修改 `large.rs` 为 `large/mod.rs`**

```rust
// mod.rs — 外观，重新导出子模块的公共 API
pub mod messages;
pub mod reactions;
pub mod reads;

pub use messages::send_message;
pub use reactions::add_reaction;
// 保持原模块的 pub API 签名完全兼容
```

**c) 逐块移动代码到子文件**

子文件内容示例：

```rust
// large/messages.rs — 消息相关的所有函数
use crate::*; // 如果使用 crate:: 路径
// 或重新导入 shared types

pub fn send_message(...) -> Result<...> { ... }
pub fn edit_message(...) -> Result<...> { ... }
pub fn delete_message(...) -> Result<...> { ... }
```

### 3. 共享状态处理

大型文件通常依赖共享状态。提取时的处理策略：

| 模式 | 做法 |
|---|---|
| `Arc<AppState>` 参数 | 保留在子模块函数签名中，或通过 `pub(crate)` 常量 |
| `use super::*` 的辅助函数 | 抽取到 `mod.rs` 或 `util.rs` 子模块 |
| 内部 `struct`/`enum` | 如果子模块独占 → 迁入子模块；如果共享 → 留在 `mod.rs` 或迁到 `types.rs` |
| cfg(test) 测试 | 测试移到对应子文件的 `#[cfg(test)] mod tests` 内 |

### 4. 测试迁移

- 每个子文件带自己的 `#[cfg(test)] mod tests { ... }`
- `db_tests.rs` 如果存在且内容与子模块相关，拆分为 `{submodule}/db_tests.rs`
- 拆分后确保 `cargo test --workspace --lib` 通过

### 5. 输出检查

拆分完成后验证：

```bash
# 编译
cargo check --workspace --quiet

# 全库测试
cargo test --workspace --lib --quiet

# 文件尺寸
bash scripts/file-size-check.sh

# clippy
cargo clippy --workspace --all-targets --quiet 2>&1
```

### 6. 输出格式

每次重构必须输出以下结构化信息：

```
[REFACTOR] 文件: src/large.rs → src/large/
[REFACTOR] 拆分方案:
  - messages.rs: send_message, edit_message, delete_message (142 行)
  - reactions.rs: add_reaction, remove_reaction, list_reactions (89 行)
  - reads.rs: mark_read, get_read_receipts, get_unread_count (67 行)
[REFACTOR] 行数变化: 723 → 98(mod.rs) + 142 + 89 + 67 = 396 (-327 行)
[REFACTOR] 测试: 未新增失败, clippy 零新 warning
[REFACTOR] 外部 API: 完全兼容（pub use 重新导出）
```
