#!/usr/bin/env bash
# file-size-check.sh — 检测文件尺寸超限
# 被 HARNESS.md 引用，每次 agent edit/write 后自动运行
set -euo pipefail

MAX_LINES=800
HARD_LIMIT=1200
violations=0
warnings=0

echo "=== File Size Check ==="

# Rust 源文件
while read -r f; do
    lines=$(wc -l < "$f")
    basename=$(basename "$f")

    # routes.rs 豁免：Axum 路由注册天然密集
    if [[ "$basename" == "routes.rs" ]]; then
        if [ "$lines" -gt 3000 ]; then
            echo "  ❌ HARD (routes): $f ($lines 行) — 超 3000 行豁免线"
            violations=$((violations + 1))
        fi
        continue
    fi

    if [ "$lines" -gt "$HARD_LIMIT" ]; then
        echo "  ❌ HARD: $f ($lines 行) — > $HARD_LIMIT 行，禁止修改，必须先拆分"
        violations=$((violations + 1))
    elif [ "$lines" -gt "$MAX_LINES" ]; then
        echo "  ⚠️  WARN: $f ($lines 行) — > $MAX_LINES 行，新增功能前先拆分"
        warnings=$((warnings + 1))
    fi
done < <(find crates -name '*.rs')

# 前端 JS
while read -r f; do
    lines=$(wc -l < "$f")
    if [ "$lines" -gt 1000 ]; then
        echo "  ❌ HARD (JS): $f ($lines 行)"
        violations=$((violations + 1))
    elif [ "$lines" -gt 600 ]; then
        echo "  ⚠️  WARN (JS): $f ($lines 行) — 建议拆分"
        warnings=$((warnings + 1))
    fi
# Exclude vendored deps: node_modules is third-party and not subject to our
# size governance (matches scripts/web-check.sh, which already prunes it).
done < <(find web -name '*.js' -not -path '*/node_modules/*')

echo "---"
echo "结果: $violations 违规, $warnings 警告"
exit $violations
