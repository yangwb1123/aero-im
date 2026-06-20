# Skill: Testing in Aero IM

**用途**：Aero IM 的测试策略和模式，保证每次提交不影响已有测试。

---

## 测试层次

### 1. 纯函数测试（`#[test]`）

最优先写。无需 DB，无需网络。

**位置**：跟在待测代码同文件的 `#[cfg(test)] mod tests { ... }` 中。

```rust
// 在 model.rs 末尾
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_searchable_text_extracts_content_only() {
        let blocks = vec![
            Block::Text { content: "hello".into(), spans: vec![] },
        ];
        assert_eq!(searchable_text(&blocks), "hello");
    }
}
```

**适合**：`aero-common`（model.rs, ids.rs, markdown.rs）、`aero-im-core`（validation.rs, spam_guard.rs, pii_detect.rs）、工具函数

### 2. 集成测试（`#[ignore]`, `#[sqlx::test]`）

需要 Postgres 连接。

**位置**：`aero-storage/src/db_tests.rs` 或对应模块的 `#[cfg(test)] mod db_tests { ... }`

**必须**：
- 标记 `#[ignore]`（需要 `--ignored` 或 `--include-ignored` 运行）
- 读取 `DATABASE_URL` 环境变量
- 使用**全新一次性库**（`CREATE DATABASE`），不共享 dev DB

```rust
#[ignore]
#[sqlx::test]
async fn test_message_insert_and_read(pool: PgPool) {
    let repo = MessageRepo::new(pool);
    // ... test logic
}
```

**适合**：`aero-storage`（所有 repo）、`aero-im-core` 的 db_tests.rs

### 3. Hermetic 单元测试

不依赖外部服务。Mock 接口使用 trait。

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spam_guard_allows_normal_message() {
        let guard = SpamGuard::new(SpamThresholds::default());
        let decision = guard.check("normal message", &ParticipantId::new_v4());
        assert_eq!(decision, SpamDecision::Allow);
    }
}
```

---

## 运行测试的命令

```bash
# 纯单元测试（最快，819 个）
cargo test --workspace --lib

# 含集成测试（需要 DATABASE_URL）
DATABASE_URL=postgres://aero:aero@localhost:5432/aero_test \
  cargo test --workspace -- --include-ignored

# 单个 crate
cargo test -p aero-im-core --lib

# 单个测试
cargo test --workspace --lib test_spam_guard_allows_normal_message
```

## 编写新测试的规则

1. **每新增一个 `pub fn`**，必须有至少一个对应的 `#[test]`
2. **错误路径必须测试**（空输入、非法参数、边界值）
3. **重构时不要减少测试数量**（`cargo test --workspace --lib` 的计数必须 ≥ 重构前）
4. **`#[ignore]` 测试如果新增失败** → 检查是否要改 fixture 而不是跳过
5. **测试命名**：`{tested_fn}_{condition}`，如 `send_message_rejects_empty_content`
