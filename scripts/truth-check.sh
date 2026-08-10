#!/usr/bin/env bash
# truth-check.sh — 检测「写了但没接线」的死代码
#
# 本项目反复出现一类失败：功能被标记为「完成」，实际是死代码。
# cargo check 抓不到这类问题，因为：
#   - 孤儿模块（源文件存在，但从未被 `mod foo;` 或 `include!` 引用）根本不参与编译；
#   - 零调用 builder（`with_xxx()` 有定义但没有任何调用方）是合法 Rust。
# 本脚本把这两类「写了但没接线」变成可见的红灯。
#
# 检测项：
#   1. ORPHAN MODULE（硬违规，计入 exit 码）：
#      crates/*/src/ 下有实质内容的 .rs 文件，在同 crate 内没有任何
#      `mod foo;` / `pub mod foo;` / `pub(crate) mod foo;` 声明或
#      `include!("path.rs")` 引用它。
#   2. UNWIRED BUILDER（⚠️ 警告，不计入 exit 码）：
#      `fn with_xxx(` builder 方法在全仓没有任何 `.with_xxx(` 调用点。
#
# 注意：当前已知 participant_cache / notification_bundle 尚未接线，会让本脚本红。
# 因此本脚本只作为独立的 `make check-truth` 目标，**暂不**折入 check-harness。
# 待 participant_cache / notification_bundle 接线后（见 docs/sprint），可折入 check-harness。
set -euo pipefail

# §3 claim-contract guard（AC4 字面量/类型/usage 单源）住在可 source 的库里；
# 本脚本只负责接线 + 把违规数折入退出码（F5：`exit orphan + guard`）。
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/truth-check-lib.sh"

orphan_violations=0
unwired_warnings=0

echo "=== Truth Check（写了但没接线 检测）==="
echo ""

# ---------------------------------------------------------------------------
# 1. 孤儿模块检测
# ---------------------------------------------------------------------------
echo "--- 1. 孤儿模块（ORPHAN MODULE）---"

# 判断一个文件是否「有实质内容」：去掉空行、行注释(//)、块注释残留后还有代码行。
# 返回 0 = 有实质内容，1 = 空文件或纯注释。
has_substance() {
    local f="$1"
    # 删除以可选空白开头的 // 或 //! 或 /// 行，再删空行；剩余非空行数 > 0 则有实质内容。
    local code_lines
    code_lines=$(grep -vE '^[[:space:]]*(//|/\*|\*|$)' "$f" 2>/dev/null | grep -cvE '^[[:space:]]*$' || true)
    [ "${code_lines:-0}" -gt 0 ]
}

# `include!` 的路径相对于包含它的源文件解析，不能只按候选文件名做文本
# 匹配。逐个解析同 crate 内的字符串字面量 include，并比较规范化后的
# 绝对路径，避免 `routes/handlers/*.rs` 这类合法拆分被误报为孤儿。
is_included_file() {
    local target="$1"
    local crate_src="$2"
    local target_abs include_file include_line include_path included_abs
    target_abs=$(realpath "$target")

    while IFS=: read -r include_file _ include_line; do
        include_path=$(printf '%s\n' "$include_line" \
            | sed -nE 's/.*include!\([[:space:]]*"([^"]+)".*/\1/p')
        [ -z "$include_path" ] && continue
        included_abs=$(realpath "$(dirname "$include_file")/$include_path" 2>/dev/null || true)
        if [ "$included_abs" = "$target_abs" ]; then
            return 0
        fi
    done < <(grep -rnE --include='*.rs' 'include!\([[:space:]]*"[^"]+"' "$crate_src" 2>/dev/null || true)

    return 1
}

is_path_module_file() {
    local target=$1
    local crate_src=$2
    local target_abs
    target_abs=$(realpath -m "$target")

    local declaration source attribute referenced candidate
    while IFS= read -r declaration; do
        source=${declaration%%:*}
        attribute=${declaration#*:}
        attribute=${attribute#*:}
        referenced=$(printf '%s\n' "$attribute" \
            | sed -nE 's/^[[:space:]]*#\[path[[:space:]]*=[[:space:]]*"([^"]+)"\].*/\1/p')
        [ -n "$referenced" ] || continue
        candidate=$(realpath -m "$(dirname "$source")/$referenced")
        if [ "$candidate" = "$target_abs" ]; then
            return 0
        fi
    done < <(
        grep -rHnE '^[[:space:]]*#\[path[[:space:]]*=[[:space:]]*"[^"]+"\]' \
            "$crate_src" --include='*.rs' 2>/dev/null || true
    )
    return 1
}

# 遍历每个 crate，单独处理（mod 声明只在同 crate 内查找）。
while IFS= read -r -d '' crate_src; do
    crate_dir="${crate_src%/src}"
    crate_name=$(basename "$crate_dir")

    # 收集该 crate src/ 下所有 .rs 文件
    while IFS= read -r -d '' f; do
        base=$(basename "$f")

        # 排除：crate / 模块入口、构建脚本、bin 入口、纯测试文件
        case "$base" in
            lib.rs|main.rs|mod.rs|build.rs|tests.rs)
                continue
                ;;
        esac
        # 排除 bin/ 下的二进制入口（与其 boot/ 子模块以 mod 声明组织，单独处理见下）
        # bin/ 下的非入口子模块仍需检测，所以只跳过直接位于 bin/ 的顶层文件。
        case "$f" in
            */src/bin/*)
                # bin/foo.rs（直接在 bin/ 下）是独立二进制入口 → 跳过。
                # bin/boot/repos.rs 这类子模块文件 → 继续检测。
                rel_in_bin="${f##*/src/bin/}"
                if [[ "$rel_in_bin" != */* ]]; then
                    continue
                fi
                ;;
        esac

        # 模块名：foo.rs -> foo
        mod_name="${base%.rs}"

        # 跳过空文件 / 纯注释文件（无实质内容不算「写了功能」）
        if ! has_substance "$f"; then
            continue
        fi

        # 在同 crate 内查找 `mod <name>;`（允许 pub / pub(crate) 前缀、行内多空格）
        # 或解析后指向该文件的 `include!`。
        # mod 仅匹配文件式声明（以分号结尾），不匹配内联 `mod foo {`。
        # 调用 grep 时用 || true，避免无命中触发 set -e。
        if grep -rqE "^[[:space:]]*(pub[[:space:]]+|pub\([^)]*\)[[:space:]]+)?mod[[:space:]]+${mod_name}[[:space:]]*;" "$crate_src" 2>/dev/null \
            || is_included_file "$f" "$crate_src" \
            || is_path_module_file "$f" "$crate_src"; then
            : # 已被声明，正常
        else
            echo "  ❌ ORPHAN MODULE: $f（写了但未 mod 声明，不参与编译）"
            orphan_violations=$((orphan_violations + 1))
        fi
    done < <(find "$crate_src" -name '*.rs' -print0)
done < <(find crates -maxdepth 2 -type d -name src -print0)

echo ""

# ---------------------------------------------------------------------------
# 2. 零调用 builder 检测（轻量启发式）
# ---------------------------------------------------------------------------
echo "--- 2. 零调用 builder（UNWIRED BUILDER）---"

# 只扫描函数名以 with_ 开头、且采用 builder 模式（消费 self 并链式返回）的方法。
# 判定标准：定义行形如 `fn with_xxx(... self ...`（含 pub / pub(crate) 前缀），
# 即第一个参数为 self / mut self / &mut self —— 这正是 builder 链式方法的签名，
# 同时天然排除了 `#[test] fn with_xxx()`（无 self 入参）这类测试函数噪声。
# 对每个唯一的 with_xxx 名字，统计全仓 `.with_xxx(` 的调用点数（排除 /// doc）。

# 先收集候选定义：path:line:with_name
collect_builder_defs() {
    # 匹配 `fn with_<ident>( ... self`，提取出 with_<ident>。
    # 要求 `(` 之后、`)` 之前出现 self，锁定 builder 签名，排除测试函数。
    # 同时排除 tests.rs 纯测试文件。
    grep -rnoE '\bfn[[:space:]]+with_[A-Za-z0-9_]+[[:space:]]*\([^)]*\bself\b' crates \
        --include='*.rs' 2>/dev/null \
        | grep -v '/tests\.rs:' || true
}

# 用关联数组去重 builder 名 -> 首个定义位置
declare -A def_loc
while IFS= read -r line; do
    [ -z "$line" ] && continue
    # line 形如  crates/.../service/orig.rs:320:    pub fn with_notification_bundles(mut self, ...
    # 取前两个 `:` 字段作为 path:line 位置（路径不含冒号）。
    path_field="${line%%:*}"
    rest="${line#*:}"
    line_no="${rest%%:*}"
    loc="${path_field}:${line_no}"
    # 提取 with_xxx 名字（紧跟 `fn ` 之后的标识符）
    name=$(printf '%s\n' "$line" | grep -oE 'fn[[:space:]]+with_[A-Za-z0-9_]+' | head -n1 \
        | grep -oE 'with_[A-Za-z0-9_]+' || true)
    [ -z "$name" ] && continue
    # 仅记录首个定义位置（同名多处取其一即可，调用统计是全仓的）
    if [ -z "${def_loc[$name]:-}" ]; then
        def_loc["$name"]="$loc"
    fi
done < <(collect_builder_defs)

# 对每个 builder 名统计调用点
for name in "${!def_loc[@]}"; do
    # 统计 `.with_xxx(` 出现次数，排除：
    #   - doc 注释行（以 /// 或 //! 或 // 开头）
    #   - 这是「调用」语义（前面是 `.`），定义行是 `fn with_xxx(`，本就不会被 `\.with_xxx(` 匹配
    call_count=$(grep -rhE "\.${name}[[:space:]]*\(" crates --include='*.rs' 2>/dev/null \
        | grep -vE '^[[:space:]]*(///|//!|//)' \
        | wc -l | tr -d ' ' || true)
    call_count="${call_count:-0}"
    if [ "$call_count" -eq 0 ]; then
        echo "  ⚠️  UNWIRED BUILDER: ${name} @ ${def_loc[$name]}（定义存在但零调用方）"
        unwired_warnings=$((unwired_warnings + 1))
    fi
done

echo ""

# ---------------------------------------------------------------------------
# 3. claim-contract guard（AC4 字面量/类型/usage 单源，硬违规计入 exit）
#    实现见 scripts/truth-check-lib.sh（sourceable）；负例自测见
#    scripts/test-claim-contract-guard.sh。
# ---------------------------------------------------------------------------
echo ""
claim_guard_scan "$(pwd)" || true

echo ""
echo "---"
echo "结果: ${orphan_violations} 个 ORPHAN, ${unwired_warnings} 个 UNWIRED, ${CLAIM_GUARD_VIOLATIONS} 个 CLAIM-GUARD"
echo ""
if [ "$orphan_violations" -gt 0 ]; then
    echo "ORPHAN 为硬违规：上述源文件写了但未 mod 声明，不参与编译（死代码）。"
fi
if [ "$unwired_warnings" -gt 0 ]; then
    echo "UNWIRED 为警告（不计入 exit 码）：builder 有定义但零调用方，疑似未接线。"
fi
if [ "$CLAIM_GUARD_VIOLATIONS" -gt 0 ]; then
    echo "CLAIM-GUARD 为硬违规：claim 字面量/类型/usage 脱离了 leaf 单源（见 truth-check-lib.sh 的 allowlist 契约理由）。"
fi

# ORPHAN 是硬违规计入 exit；UNWIRED 仅警告，不阻断。
# claim-contract guard 违规与 ORPHAN 一样折入 exit（F5 修正：字面量违规只打印不阻断 = 静默漂移）。
exit $((orphan_violations + CLAIM_GUARD_VIOLATIONS))
