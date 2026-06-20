#!/usr/bin/env bash
# agent-start.sh — Agent 启动时自动运行的诊断和决策脚本
# 调用方式：bash scripts/agent-start.sh
# 输出决策建议：下一个应该执行的重构任务

set -euo pipefail

REFACTOR_STEPS=(
    "crates/aero-im-core/src/service.rs"
    "crates/aero-storage/src/message.rs"
    "crates/aero-storage/src/workspace.rs"
    "crates/aero-ai/src/service.rs"
    "crates/aero-server/src/ws.rs"
    "web/app.js"
    "crates/aero-common/src/model.rs"
)

echo "=== Aero IM — Agent Startup Diagnostics ==="
echo ""

# 1. 文档完整性检查
echo "① 检查关键文档是否存在..."
for doc in "BOOTSTRAP.md" "AGENTS.md" "HARNESS.md" "docs/sprint/CURRENT_SPRINT.md" "docs/REFACTOR_PLAN.md" "docs/sprint/TODO.md"; do
    if [ -f "$doc" ]; then
        echo "  ✓ $doc ($(wc -l < $doc) 行)"
    else
        echo "  ❌ $doc 缺失"
    fi
done

# 2. 文件尺寸违规
echo ""
echo "② 检查文件尺寸违规..."
HARD_COUNT=0
HARD_FILES=""
while IFS= read -r line; do
    if echo "$line" | grep -q '❌ HARD'; then
        HARD_COUNT=$((HARD_COUNT + 1))
        filepath=$(echo "$line" | sed 's/.*❌ HARD[^:]*: //' | sed 's/ ([0-9]* 行).*//')
        echo "  HARD: $filepath"
        HARD_FILES="$HARD_FILES $filepath"
    fi
done < <(bash scripts/file-size-check.sh 2>&1 || true)

# 3. 依赖方向
echo ""
echo "③ 检查 crate 依赖方向..."
DEPS_OUTPUT=$(bash scripts/dependency-check.sh 2>&1 || true)
if echo "$DEPS_OUTPUT" | grep -qE "结果: [1-9]" 2>/dev/null; then
    echo "  ❌ 依赖方向有违规！"
    echo "$DEPS_OUTPUT" | grep "    ❌"
else
    echo "  ✓ 依赖方向合规"
fi

# 4. 自动决策：找出第一个未拆分的 Step
echo ""
echo "④ 自动决策..."
echo ""

if [ "$HARD_COUNT" -gt 0 ]; then
    # 查找 REFACTOR_PLAN.md 中第一个目标文件仍然超限的 Step
    NEXT_STEP=""
    NEXT_TARGET=""
    for i in "${!REFACTOR_STEPS[@]}"; do
        target="${REFACTOR_STEPS[$i]}"
        step=$((i + 1))
        if [ -f "$target" ]; then
            lines=$(wc -l < "$target")
            if [ "$lines" -gt 1200 ]; then
                NEXT_STEP=$step
                NEXT_TARGET="$target ($lines 行)"
                break
            fi
        fi
    done

    if [ -n "$NEXT_STEP" ]; then
        echo "  ┌─────────────────────────────────────────────────────────────────┐"
        echo "  │ 当前 $HARD_COUNT 个 HARD 违规文件                                      │"
        echo "  │ REFACTOR 优先级高于一切                                              │"
        echo "  └─────────────────────────────────────────────────────────────────┘"
        echo ""
        echo "  下一个任务: docs/REFACTOR_PLAN.md Step $NEXT_STEP — $NEXT_TARGET"
        echo ""
        echo "  操作:"
        echo "    bash scripts/refactor-worker.sh $NEXT_STEP"
        echo ""
        echo "  然后:"
        echo "    1. 读 skills/refactor-large-file.md"
        echo "    2. 按 refactor-worker.sh 指引拆分文件"
        echo "    3. 运行: make check-rebase"
        echo "    4. 运行: make init-baseline"
        echo "    5. 运行: cargo test --workspace --lib --quiet"
        echo ""
        echo "  自动迭代:"
        echo "    完成拆分后运行:"
        echo "      bash scripts/agent-loop.sh $NEXT_STEP"
    else
        echo "  ⚠️  文件尺寸检查显示 $HARD_COUNT 个违规，但未找到匹配 Step 的目标文件"
        echo "  手动检查: bash scripts/file-size-check.sh"
    fi

elif [ -f "docs/sprint/TODO.md" ] && grep -q '\[ \]' docs/sprint/TODO.md 2>/dev/null; then
    PENDING=$(grep -c '\[ \]' docs/sprint/TODO.md || true)
    echo "  ✓ 无文件尺寸违规"
    echo "  运行 task-planner.sh 查看 Phase 2 任务分配"
    echo ""
    echo "  bash scripts/task-planner.sh"
else
    echo "  ✓ 所有检查通过"
    echo "  运行 task-planner.sh 查看 Phase 2 任务分配"
    echo ""
    echo "  bash scripts/task-planner.sh"
fi

# 5. 编译状态
echo ""
echo "⑤ 最后编译状态:"
if cargo check --workspace --quiet 2>/dev/null; then
    echo "  ✓ 编译通过"
else
    echo "  ❌ 编译有错误（运行 cargo check 查看详情）"
fi
