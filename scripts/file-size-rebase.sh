#!/usr/bin/env bash
# file-size-rebase.sh — 只检查新增的文件尺寸违规（忽略已有的）
# 在 REFACTOR_PLAN 执行期间使用：重构一个文件时，不因其他 HARD 违规而阻断
set -euo pipefail

MAX_LINES=800
HARD_LIMIT=1200

# 读取已知违规基线（从 .refactor-baseline 文件）
BASELINE_FILE=".refactor-baseline"
if [ ! -f "$BASELINE_FILE" ]; then
    echo "⚠️  基线文件不存在，请先运行 'bash scripts/file-size-check.sh > .refactor-baseline'"
    echo "   这将记录当前所有违规为"已知"，后续只检查新增违规"
    exit 1
fi

violations=0
warnings=0

while read -r f; do
    lines=$(wc -l < "$f")
    basename=$(basename "$f")

    # 检查这个文件的违规是否已在基线中
    # 基线格式: "❌ HARD: path (lines) — ..."
    is_known=false
    while IFS= read -r baseline; do
        if echo "$baseline" | grep -qF "$f"; then
            is_known=true
            break
        fi
    done < "$BASELINE_FILE"

    # routes.rs 豁免
    if [[ "$basename" == "routes.rs" ]]; then
        if [ "$lines" -gt 3000 ] && [ "$is_known" = false ]; then
            echo "❌ NEW-HARD (routes): $f ($lines 行)"
            violations=$((violations + 1))
        fi
        continue
    fi

    if [ "$lines" -gt "$HARD_LIMIT" ]; then
        if [ "$is_known" = false ]; then
            echo "❌ NEW-HARD: $f ($lines 行)"
            violations=$((violations + 1))
        else
            echo "  (已知) $f ($lines 行) — 基线中，推迟处理"
        fi
    elif [ "$lines" -gt "$MAX_LINES" ]; then
        if [ "$is_known" = false ]; then
            echo "⚠️  NEW-WARN: $f ($lines 行)"
            warnings=$((warnings + 1))
        fi
    fi
done < <(find crates -name '*.rs')

# JS 文件
while read -r f; do
    lines=$(wc -l < "$f")
    is_known=false
    while IFS= read -r baseline; do
        if echo "$baseline" | grep -qF "$f"; then
            is_known=true
            break
        fi
    done < "$BASELINE_FILE"

    if [ "$lines" -gt 1000 ] && [ "$is_known" = false ]; then
        echo "❌ NEW-HARD (JS): $f ($lines 行)"
        violations=$((violations + 1))
    fi
done < <(find web -name '*.js')

echo "---"
echo "结果: $violations 新增违规, $warnings 新增警告"
if [ "$violations" -gt 0 ]; then
    echo "❌ 重构过程中产生了新的 HARD 违规！请检查拆分是否正确。"
else
    echo "✓ 无新增违规（已有违规在基线中）"
fi
exit $violations
