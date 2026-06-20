#!/usr/bin/env bash
# task-planner.sh — 任务规划器
# 读 CURRENT_SPRINT.md 看板 + 当前代码库状态 → 输出下一步行动指令
# 调用方式: bash scripts/task-planner.sh
set -euo pipefail

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║  Aero IM — Task Planner                                       ║"
echo "╚═══════════════════════════════════════════════════════════════╝"
echo ""

# 1. 检查 REFACTOR 状态
echo "① 检查代码健康状况..."
HARD_COUNT=$(bash scripts/file-size-check.sh 2>&1 | grep -c '❌ HARD' || true)
DEPS_VIOLATIONS=$(bash scripts/dependency-check.sh 2>&1 | grep -oE "结果: [0-9]+" | grep -oE "[0-9]+" || true)
COMPILE_OK=true
cargo check --workspace --quiet 2>/dev/null || COMPILE_OK=false

echo "  文件尺寸违规: $HARD_COUNT HARD"
echo "  依赖违规:     ${DEPS_VIOLATIONS:-0}"
echo "  编译:         $($COMPILE_OK && echo '✓' || echo '❌')"
echo ""

# 2. 决策
echo "② 任务决策..."
echo ""

if [ "$HARD_COUNT" -gt 0 ]; then
    # Phase 1: REFACTOR
    echo "  📋 分配到: Phase 1 — 代码重构"
    echo "  原因: $HARD_COUNT 个文件超过 1200 行硬限制"
    echo ""
    echo "  下一个任务:"
    echo "    bash scripts/agent-start.sh"
    echo "    → 自动选择下一个需要拆分的 docs/REFACTOR_PLAN.md Step"
    echo ""

elif [ "$DEPS_VIOLATIONS" -gt 0 ]; then
    # 依赖违规优先于新功能
    echo "  📋 分配到: 依赖修复"
    echo "  原因: $DEPS_VIOLATIONS 个依赖方向违规"
    echo ""
    echo "  下一个任务:"
    echo "    bash scripts/dependency-check.sh"
    echo "    → 修复反向依赖违规"
    echo ""

elif ! $COMPILE_OK; then
    echo "  📋 分配到: 编译修复 (STOP)"
    echo "  原因: 编译失败，不能继续开发"
    echo ""
    echo "  下一个任务:"
    echo "    cargo check --workspace"
    echo "    → 修复编译错误"
    echo ""

else
    # Phase 2: 功能开发
    echo "  📋 分配到: Phase 2 — 功能开发"
    echo "  原因: 所有代码健康检查通过"
    echo ""

    # 读取 docs/sprint/CURRENT_SPRINT.md 找第一个 [ ] 任务
    FIRST_TASK=$(grep '\[ \]' docs/sprint/CURRENT_SPRINT.md 2>/dev/null | head -3 || true)
    if [ -n "$FIRST_TASK" ]; then
        echo "  下一个任务 (来自 docs/sprint/CURRENT_SPRINT.md):"
        echo ""
        while IFS= read -r line; do
            echo "    $line"
        done < <(echo "$FIRST_TASK")
        echo ""
        echo "  完整看板: cat docs/sprint/CURRENT_SPRINT.md"
    else
        echo "  所有 Phase 2 任务已完成!"
        echo "  考虑启动 Phase 3（开放平台）"
        echo "  读 docs/ROADMAP.md 做战略决策"
    fi
fi

echo "③ 当前 Sprint 摘要:"
echo ""
grep -E '^\| \[ \]|^\| \[x\]|^📊' docs/sprint/CURRENT_SPRINT.md 2>/dev/null | head -10 || true
echo ""
echo "---"
echo "运行 cat docs/sprint/CURRENT_SPRINT.md 查看完整看板"
