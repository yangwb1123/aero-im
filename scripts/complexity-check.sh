#!/usr/bin/env bash
# complexity-check.sh — 检测超长函数（> 50 行）
# 近似检测：连续非空行数超出阈值即报告
set -euo pipefail

MAX_LINES=50
violations=0

echo "=== Long Function Check ==="
echo "（仅提供近似行号，实际函数边界需人工确认）"

find crates -name '*.rs' | while read -r f; do
    # 匹配函数定义，跟踪其体长度
    awk -v file="$f" -v max="$MAX_LINES" '
    /^pub (unsafe )?async fn |^pub (unsafe )?fn |^async fn |^fn |^pub fn / {
        func_name = $0
        func_start = NR
        brace_count = 0
        lines_in_func = 0
        in_func = 0
    }
    in_func == 0 && func_start != "" && /{/ {
        in_func = 1
        brace_count = 1
        lines_in_func = 1
        next
    }
    in_func == 1 {
        lines_in_func++
        # count braces
        for (i = 1; i <= length($0); i++) {
            c = substr($0, i, 1)
            if (c == "{") brace_count++
            if (c == "}") brace_count--
        }
        if (brace_count <= 0) {
            if (lines_in_func > max) {
                printf "  ⚠️  LONG: %s:%d (%d 行) — %s\n", file, func_start, lines_in_func, func_name
            }
            in_func = 0
            func_start = ""
            lines_in_func = 0
        }
    }
    ' "$f"
done

echo "---"
echo "检查完成（圈复杂度检查需人工介入）"
