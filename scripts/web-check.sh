#!/usr/bin/env bash
# web-check.sh — 前端静态兼容性校验门
#
# web/ 使用 SolidJS/Vite；本门保持沙箱可跑、无需 npm install，负责保留的
# 原生 JS 辅助模块的语法/相对 import，以及 SolidJS 入口 HTML 的本地引用；
# SolidJS 的 TSX 类型与生产构建由
# `cd web && pnpm run build` 负责。
#
# 检测项（逐个 web/**/*.js）：
#   1. 语法     —— node --check <file>
#   2. import 解析 —— 每个 import ... from '相对路径'（./x.js / ../x.js）
#                    校验目标文件确实存在于磁盘
#   3. index.html 的 <script type="module" src="..."> 本地引用存在
#
# 退出码 = violation 数（0 = 通过）。被 Makefile `check-web` 与 CI `web-check` 调用。
set -euo pipefail

# --- 定位仓库根（脚本在 scripts/ 下）---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WEB_DIR="$ROOT_DIR/web"

violations=0
checked_files=0
checked_imports=0

echo "=== Web Check（前端静态校验门）==="

# --- node 优雅缺失处理 ---
if ! command -v node >/dev/null 2>&1; then
  echo "  ⚠️  未检测到 node —— 跳过前端语法/import 校验"
  echo "      （本门需要 node 才能运行；CI/沙箱请确保 node 在 PATH 中）"
  echo "---"
  echo "结果: 0 违规（node 缺失，已跳过）"
  exit 0
fi

if [ ! -d "$WEB_DIR" ]; then
  echo "  ⚠️  未找到 web/ 目录（$WEB_DIR）—— 无可校验文件"
  echo "---"
  echo "结果: 0 违规"
  exit 0
fi

# --- 解析相对 import 目标：给定 来源文件 + 相对路径，回显磁盘上的绝对目标路径 ---
resolve_relative() {
  local src_file="$1" spec="$2"
  local src_dir
  src_dir="$(cd "$(dirname "$src_file")" && pwd)"
  # 用纯 shell 规范化（避免依赖 realpath --relative-to 等不可移植选项）
  printf '%s/%s\n' "$src_dir" "$spec"
}

# =====================================================================
# 1) + 2) 逐个 .js：语法 + import 解析
# =====================================================================
while IFS= read -r f; do
  checked_files=$((checked_files + 1))

  # --- 1. 语法 ---
  # 注意：必须以 ESM 模式校验。直接 `node --check file.js` 会把 .js 当 CommonJS
  # 解析，对 ESM-only 写法（顶层 import/export）过于宽松，会漏掉真实语法错误
  # （实测 node v26：`export function f( {` 这类破损 `--check file.js` 返回 0）。
  # 改用 `--input-type=module` 经 stdin 喂入，强制 ESM 解析，真实报错。
  if ! node --check --input-type=module < "$f" >/tmp/web-check-syntax.$$ 2>&1; then
    rel_src="${f#"$ROOT_DIR"/}"
    echo "  ❌ SYNTAX ERROR: $rel_src"
    # node 报错里源标作 [stdin]，替换回真实文件名便于定位
    sed "s#\[stdin\]#$rel_src#g; s/^/        /" /tmp/web-check-syntax.$$
    violations=$((violations + 1))
  fi

  # --- 2. import 解析（含多行 import { ... } from '...';）---
  # 思路：把整个文件的换行折叠成空格后，用 grep -oE 抽出所有
  #   from '相对路径'  /  import '相对路径'（副作用 import）
  # 只关心相对路径（./ 或 ../ 开头）；裸模块名（如 CDN、bare specifier）不校验存在性。
  while IFS= read -r spec; do
    [ -z "$spec" ] && continue
    checked_imports=$((checked_imports + 1))
    target="$(resolve_relative "$f" "$spec")"
    if [ ! -f "$target" ]; then
      # 显示规范化后的可读路径（去掉 ROOT_DIR 前缀）
      rel_src="${f#"$ROOT_DIR"/}"
      echo "  ❌ UNRESOLVED IMPORT: $rel_src → $spec  (期望文件不存在: $target)"
      violations=$((violations + 1))
    fi
  done < <(
    tr '\n' ' ' < "$f" \
      | grep -oE "(from|import)[[:space:]]+['\"](\.\.?/)[^'\"]+['\"]" \
      | grep -oE "['\"](\.\.?/)[^'\"]+['\"]" \
      | sed -E "s/^['\"]//; s/['\"]\$//"
  )
done < <(find "$WEB_DIR" -name '*.js' -type f \
  -not -path '*/node_modules/*' -not -path '*/dist/*' | sort)

# =====================================================================
# 3) (轻量) frontend entry HTML's module scripts
# =====================================================================
for HTML in "$WEB_DIR/index.html"; do
  [ -f "$HTML" ] || continue
  while IFS= read -r src; do
    [ -z "$src" ] && continue
    # 跳过绝对 URL（http(s):// 或 //cdn...）
    case "$src" in
      http://*|https://*|//*) continue ;;
    esac
    target="$(resolve_relative "$HTML" "$src")"
    if [ ! -f "$target" ]; then
      rel_html="${HTML#"$ROOT_DIR"/}"
      echo "  ❌ UNRESOLVED SCRIPT: $rel_html → $src  (期望文件不存在: $target)"
      violations=$((violations + 1))
    fi
  done < <(
    grep -oE "<script[^>]*type=['\"]module['\"][^>]*src=['\"][^'\"]+['\"]" "$HTML" \
      | grep -oE "src=['\"][^'\"]+['\"]" \
      | sed -E "s/^src=['\"]//; s/['\"]\$//"
  )
done

rm -f /tmp/web-check-syntax.$$

echo "---"
echo "已检查: $checked_files 个 .js 文件, $checked_imports 个相对 import"
echo "结果: $violations 违规"
exit "$violations"
