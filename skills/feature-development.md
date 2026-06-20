# Skill: Feature Development in Aero IM

**用途**：在 Phase 2（功能开发模式）下，按标准化步骤添加新功能。
**触发条件**：`bash scripts/task-planner.sh` 输出 "Phase 2 — 功能开发"

---

## 步骤 1: 确认范围和影响

```bash
# 当前任务来自 docs/sprint/CURRENT_SPRINT.md 的 [ ] 列表
cat docs/sprint/CURRENT_SPRINT.md | grep '\[ \]' | head -3
```

**检查**：
- 任务涉及的 crate 是哪个？读 `skills/clean-architecture.md` 确认放置位置
- 是否需要新 SQL 迁移？AGENTS.md §3.1: "迁移 → build → migrate" 顺序
- 是否影响现有测试？`cargo test --workspace --lib` 跑一遍记下当前测试数

## 步骤 2: 读相关 Context

取决于任务类型：

| 任务类型 | 需要读的文件 |
|---|---|
| 新数据表 | `skills/clean-architecture.md`（放哪个 crate）+ `AGENTS.md §3.1`（迁移规范） |
| 新 API 端点 | 看同 crate 现有 handler 模式 + `AGENTS.md §3.2`（鉴权用 AuthUser） |
| 新 WS 事件 | `model.rs` 的 RoomEvent/StreamEvent + hub.rs 的 fan_out_raw |
| AI 功能 | `aero-ai/src/service.rs` 的 AiService 模式 |
| 存储层 | 对应 `aero-storage/src/XRepo.rs` 模式（仓储 + sqlx） |

## 步骤 3: 执行实现

按 AGENTS.md §3.1 的"加功能配方"：

```
1. 迁移 → 2. 仓储 → 3. HTTP/WS → 4. 鉴权 → 5. 实时事件
```

### 具体规范

**迁移文件** `migrations/NNNN_descriptive_name.sql`：
```sql
CREATE TABLE IF NOT EXISTS new_table (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- 所有表都有 created_at
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- 幂等：所有 DDL 用 IF NOT EXISTS / IF EXISTS
```

**仓储层** `aero-storage/src/new_feature.rs`：
```rust
pub struct NewFeatureRepo {
    pool: PgPool,
}

impl NewFeatureRepo {
    pub fn new(pool: PgPool) -> Self { Self { pool } }

    pub async fn insert(&self, ...) -> Result<...> { ... }
    pub async fn get(&self, ...) -> Result<...> { ... }
    // 每个 pub fn 必须有至少一个对应的 #[test]
}
```

**HTTP handler**（在 `aero-server/src/new_feature.rs`）：
```rust
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/feature", post(handler))
}

async fn handler(
    State(s): State<AppState>,
    auth: AuthUser,
    Json(body): Json<FeatureRequest>,
) -> ApiResult<Json<FeatureResponse>> {
    // 1. 鉴权（AuthUser extractor 自动完成）
    // 2. 业务逻辑（委托给 ImService 或直接操作仓储）
    // 3. 返回响应
}
```

**实时事件**（如果需要 WS 扇出）：
```rust
// 在 service.rs 中 publish RoomEvent::NewVariant
s.bus.publish_room_event(room_id, RoomEvent::NewVariant { ... }).await?;
// hub.run_bus_listener 会自动解码并 fan_out_raw
```

## 步骤 4: 验证

```bash
# 1. 编译
cargo check --workspace --quiet

# 2. 测试（确保测试数 >= 实现前）
cargo test --workspace --lib --quiet

# 3. Harness 门控
make check-harness

# 4. lint
cargo clippy --workspace --all-targets --quiet 2>&1
```

## 步骤 5: 更新任务看板

```bash
# 在 docs/sprint/CURRENT_SPRINT.md 中将对应 [ ] 改为 [x]
# 然后重新运行 task-planner 获取下一个任务
bash scripts/task-planner.sh
```

## 什么情况下应该 STOP（而不是继续写代码）

| 条件 | 动作 |
|---|---|
| `cargo check` 失败 | STOP — 必须修复 |
| `make check-harness` 不通过（HARD 违规增加） | STOP — 说明新代码产生了尺寸违规，先拆分 |
| `make check-harness` 不通过（依赖违规） | STOP — 说明放错了 crate |
| 发现需要修改的文件 > 800 行 | STOP — 先执行 docs/REFACTOR_PLAN.md 对应 Step |
| 新功能导致 clippy warning 增加 | WARN — 修复 warning 后再继续 |
| 测试总数减少 | STOP — 可能不小心删除了测试 |
