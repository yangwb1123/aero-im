#!/usr/bin/env bash
# crate-dependencies.sh — 可视化所有 crate 的依赖关系
# 输出格式：crate_name → dep1, dep2, ...
set -euo pipefail

echo "=== Aero IM Crate Dependencies ==="
echo ""

ORDER=(
    "aero-common"
    "aero-bus"
    "aero-storage"
    "aero-auth"
    "aero-signaling"
    "aero-live-core"
    "aero-push"
    "aero-live-hls"
    "aero-live-rtmp"
    "aero-live-whip"
    "aero-live-webrtc"
    "aero-live-srt"
    "aero-im-core"
    "aero-im-call"
    "aero-ai"
    "aero-server"
)

for crate in "${ORDER[@]}"; do
    path="crates/$crate"
    if [ ! -f "$path/Cargo.toml" ]; then
        echo "  ⚠️  $crate — (Cargo.toml 不存在)"
        continue
    fi

    deps=$(grep -oE 'aero-[a-z-]+' "$path/Cargo.toml" 2>/dev/null | grep -v "^$crate$" | sort -u | tr '\n' ' ' | xargs || true)

    if [ -n "$deps" ]; then
        echo "  $crate → $deps"
    else
        echo "  $crate → (叶子)"
    fi
done

echo ""
echo "运行 scripts/dependency-check.sh 执行正式依赖审计。"
