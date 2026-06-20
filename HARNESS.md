# HARNESS.md — Agent 自动检查与拒绝策略

> 每次提交/编辑前，Agent 必须运行 `make check-harness`。
> 不通过 → **立即停止新增功能** → 自动重构 → 重新检查 → 通过后继续。

## 检查链（按顺序）

```bash
make check-harness
# 等价于:
#   1. cargo check --workspace --all-targets      # 编译
#   2. bash scripts/file-size-check.sh            # 文件尺寸
#   3. bash scripts/complexity-check.sh           # 函数复杂度
#   4. bash scripts/dependency-check.sh           # 依赖方向
```

### 完整提交门控

```bash
make check-full
# 等价于 check-harness +:
#   5. cargo clippy --workspace --all-targets     # 代码风格
#   6. cargo test --workspace --lib               # 测试
```

## 拒绝策略

| 检查 | 不通过时处理 |
|---|---|
| `cargo check` 失败 | **STOP** — 修复编译错误 |
| 文件超 HARD_LIMIT(1200行) | **STOP** — 创建 REFACTOR TASK，优先级高于所有新功能 |
| 函数超 50 行 | **STOP** — 提取子函数后继续 |
| 依赖方向违规 | **STOP** — 撤销导致反向依赖的修改 |
| clippy warning 增加 | **WARN** — 撤销导致新 warning 的代码 |
| 测试新增失败 | **STOP** — 回退导致测试失败的修改 |
| 文件超 WARNING 线(800行) | **DISCIPLINED** — 新增功能前先拆分受影响的文件 |

## 提交输出格式

Agent 每次修改后输出结构化摘要：

```
[HOOK] Modified: crates/aero-server/src/routes.rs (+42/-8)
[HOOK] 行数:     routes.rs(2635) service.rs(2418)  ← 当前 > 800 的文件
[HOOK] 检查:     ✓ check  ✓ size  ✓ complexity  ✓ deps
[HOOK] 重构优先级: next → 拆分 service.rs 到 service/{messages,reactions,reads}.rs
```

## Hooks 配置（Claude Code）

Bot 配置文件（`$PROJECT/.claude/settings.json`）：

```json
{
  "hooks": {
    "post_edit": [
      "make check-harness"
    ]
  }
}
```

无 post_edit hook 的 Agent 须**人工记忆**每次 edit 后运行 `make check-harness`。

## 相关文件索引

| 文件 | 作用 |
|---|---|
| `BOOTSTRAP.md` | Agent 入口，第一帧上下文 |
| `AGENTS.md §3.3` | 文件尺寸的硬约束定义 |
| `docs/sprint/CURRENT_SPRINT.md` | 当前 Sprint 目标（P0_REFACTOR 优先） |
| `skills/refactor-large-file.md` | 拆文件的操作指南 |
| `skills/modularization.md` | 模块组织规范 |
| `skills/clean-architecture.md` | crate 依赖规则 |
| `scripts/file-size-check.sh` | 尺寸检测脚本 |
| `scripts/complexity-check.sh` | 复杂度检测脚本 |
| `scripts/dependency-check.sh` | 依赖方向合规审计 |
| `scripts/crate-dependencies.sh` | 依赖关系可视化 |
