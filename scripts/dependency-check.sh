#!/usr/bin/env bash
# dependency-check.sh — 检查 Aero IM crate 间的依赖方向是否合规
# 根据 AGENTS.md §1 和 skills/clean-architecture.md 定义的依赖规则
set -euo pipefail

violations=0
echo "=== Dependency Check ==="

# aero-common 必须为叶子（不能依赖任何其他 aero crate）
echo "  1. aero-common → 无内部依赖?"
if grep -qE 'aero-(bus|storage|auth|signaling|im|live|push|server)' crates/aero-common/Cargo.toml 2>/dev/null; then
    echo "    ❌ aero-common 依赖了其他内部 crate"
    violations=$((violations + 1))
else
    echo "    ✓ OK"
fi

# 辅助函数：提取 dep 并排掉允许列表
check_deps() {
    local crate=$1
    local allowlist=$2  # 逗号分隔
    local path="crates/$crate/Cargo.toml"

    if [ ! -f "$path" ]; then
        echo "    ⚠️  $path 不存在，跳过"
        return 0
    fi

    IFS=',' read -ra ALLOW <<< "$allowlist"
    # 构建 grep -vE 的模式
    local exclude="$crate"
    for a in "${ALLOW[@]}"; do
        exclude="$exclude|$a"
    done

    # Parse dependency keys only. Grepping the entire TOML also sees vendor
    # paths such as `vendor/aero-str0m-msrv`, which are not workspace crates.
    DEPS=$(sed -nE \
        's/^[[:space:]]*(aero-[a-z0-9-]+)[[:space:]]*(\.|=).*$/\1/p' \
        "$path" | grep -vE "^($exclude)$" | sort -u || true)
    if [ -n "$DEPS" ]; then
        echo "    ❌ $crate 依赖了: $(echo "$DEPS" | tr '\n' ' ')"
        return 1
    fi
    echo "    ✓ OK"
    return 0
}

check_deps "aero-bus" "aero-common" || violations=$((violations + 1))
check_deps "aero-storage" "aero-common" || violations=$((violations + 1))
check_deps "aero-auth" "aero-common,aero-storage" || violations=$((violations + 1))
check_deps "aero-signaling" "aero-common" || violations=$((violations + 1))
check_deps "aero-live-core" "aero-common,aero-storage" || violations=$((violations + 1))
check_deps "aero-push" "aero-common" || violations=$((violations + 1))
check_deps "aero-live-hls" "aero-common,aero-live-core" || violations=$((violations + 1))
check_deps "aero-live-rtmp" "aero-common,aero-live-core,aero-live-hls,aero-storage" || violations=$((violations + 1))
check_deps "aero-live-whip" "aero-common,aero-live-core,aero-live-hls,aero-signaling,aero-storage" || violations=$((violations + 1))
check_deps "aero-live-webrtc" "aero-common,aero-live-core,aero-signaling" || violations=$((violations + 1))
check_deps "aero-live-srt" "aero-common,aero-live-core,aero-live-hls,aero-storage" || violations=$((violations + 1))
check_deps "aero-im-core" "aero-common,aero-bus,aero-storage,aero-auth,aero-signaling" || violations=$((violations + 1))
check_deps "aero-im-call" "aero-common,aero-storage,aero-signaling,aero-live-webrtc" || violations=$((violations + 1))
check_deps "aero-ai" "aero-common,aero-storage,aero-bus" || violations=$((violations + 1))

# 检查是否有 crate 依赖了 aero-server（禁止反向依赖）
echo "  检查反向依赖（任何 crate → aero-server）?"
FOUND_ILLEGAL=false
for crate in crates/*/; do
    name=$(basename "$crate")
    [ "$name" = "aero-server" ] && continue
    if grep -q 'aero-server' "$crate/Cargo.toml" 2>/dev/null; then
        echo "    ❌ $name 依赖了 aero-server"
        violations=$((violations + 1))
        FOUND_ILLEGAL=true
    fi
done
if [ "$FOUND_ILLEGAL" = false ]; then
    echo "    ✓ OK（无非法反向依赖）"
fi

echo "---"
echo "结果: $violations 依赖违规"
exit $violations
