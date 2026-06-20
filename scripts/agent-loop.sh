#!/usr/bin/env bash
# agent-loop.sh — 自动重构迭代驱动
# 每轮迭代：检查违规 → 执行一步重构 → 验证 → 更新基线 → 报告进度
# 调用方式：bash scripts/agent-loop.sh
# 适合无人值守场景：Agent 每完成一轮重构后调用此脚本获取下一步指令

set -euo pipefail

ITERATION=${1:-1}
MAX_ITERATIONS=${2:-10}  # 安全上限，防止无限循环

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║  Aero IM — Refactor Iteration #$ITERATION                       ║"
echo "╚═══════════════════════════════════════════════════════════════╝"
echo ""

# 防止无限迭代
if [ "$ITERATION" -gt "$MAX_ITERATIONS" ]; then
    echo "❌ 达到最大迭代次数 ($MAX_ITERATIONS)，停止"
    exit 0
fi

# 1. 运行诊断
echo "① 诊断当前状态..."
HARD_COUNT=0
while IFS= read -r line; do
    if echo "$line" | grep -q '❌ HARD'; then
        HARD_COUNT=$((HARD_COUNT + 1))
    fi
done < <(bash scripts/file-size-check.sh 2>&1 || true)

# 2. 编译检查
echo "② 编译检查..."
if ! cargo check --workspace --quiet 2>/dev/null; then
    echo "  ❌ 编译失败！请修复后再继续"
    exit 1
fi
echo "  ✓ 编译通过"

# 3. 测试检查
echo "③ 测试检查..."
if ! cargo test --workspace --lib --quiet 2>/dev/null; then
    echo "  ❌ 测试失败！请修复后再继续"
    exit 1
fi
echo "  ✓ 测试通过 ($(grep -c 'ok' <(cargo test --workspace --lib --quiet 2>&1 || true) || true) 个测试)"

# 4. 基线更新
echo "④ 更新尺寸基线..."
make init-baseline 2>&1 | tail -1
echo ""

# 5. 进度报告
echo "⑤ 重构进度:"
echo ""
echo "  HARD 违规剩余: $HARD_COUNT"
echo "  已完成迭代:    $((ITERATION - 1))"
echo ""

if [ "$HARD_COUNT" -eq 0 ]; then
    echo "  ┌─────────────────────────────────────────────────────────────────┐"
    echo "  │ 🎉 所有 HARD 违规已清除！                                       │"
    echo "  │ 可以开始处理 docs/sprint/TODO.md 中的待办项                          │"
    echo "  └─────────────────────────────────────────────────────────────────┘"
    echo ""
    echo "  下一步: 读 docs/sprint/CURRENT_SPRINT.md → 处理 docs/sprint/TODO.md"
    echo ""
    # 归档迭代日志
    echo "  共 $ITERATION 轮迭代完成" > .refactor-log
    echo "  最终违规: 0 HARD" >> .refactor-log
    exit 0
fi

echo "  ┌─────────────────────────────────────────────────────────────────┐"
echo "  │ 仍有 $HARD_COUNT 个 HARD 违规，继续下一轮重构                           │"
echo "  └─────────────────────────────────────────────────────────────────┘"
echo ""
echo "  下一步:"
echo "    bash scripts/agent-start.sh"
echo "    # 或者直接运行："
echo "    # 查找下一个超限的 docs/REFACTOR_PLAN.md Step 并执行"

# 6. 写出当前违规文件列表（供后续迭代参考）
bash scripts/file-size-check.sh 2>&1 | grep '❌ HARD' | sed 's/.*❌ HARD[^:]*: //' | sed 's/ ([0-9]* 行).*//' > .current-violations
echo ""
echo "  当前违规文件列表已保存到 .current-violations"
echo "  ($(wc -l < .current-violations) 个文件)"
echo ""
echo "  下一轮迭代: bash scripts/agent-loop.sh $((ITERATION + 1))"
