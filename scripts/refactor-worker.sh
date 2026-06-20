#!/usr/bin/env bash
# refactor-worker.sh — 自动执行 docs/REFACTOR_PLAN.md 中的一个 Step
# 用法: bash scripts/refactor-worker.sh [step-number]
# 示例: bash scripts/refactor-worker.sh 1   # 拆分 service.rs
set -euo pipefail

STEP=${1:-1}

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║  Aero IM — Refactor Worker                                   ║"
echo "║  自动执行 docs/REFACTOR_PLAN.md Step $STEP                     ║"
echo "╚═══════════════════════════════════════════════════════════════╝"
echo ""

case $STEP in
  1)
    TARGET="crates/aero-im-core/src/service.rs"
    TARGET_DIR="crates/aero-im-core/src/service"
    LINES=$(wc -l < "$TARGET")
    echo "Step 1: 拆分 $TARGET ($LINES 行) → $TARGET_DIR/"
    echo ""
    echo "  这个文件包含 ImService 的所有方法。"
    echo "  预计拆分为:"
    echo "    service/mod.rs          — ImService struct + pub use 重新导出"
    echo "    service/messages.rs     — send_message, edit_message, delete_message"
    echo "    service/reactions.rs    — add_reaction, remove_reaction, list_reactions"
    echo "    service/reads.rs        — mark_read, get_unread_count, get_read_receipts"
    echo "    service/notifications.rs — send_notification, notify_batch"
    echo ""
    echo "  操作指引（人工/Agent 执行）："
    echo "  ────────────────────────────────────────────"
    echo "  1. 创建 $TARGET_DIR/"
    echo "  2. 从 $TARGET 中识别各功能的函数块"
    echo "  3. 逐块移动到对应子文件"
    echo "  4. 创建 service/mod.rs 做 pub use 重新导出"
    echo "  5. 删除原 $TARGET（或重写为 mod service; pub use service::*;）"
    echo "  6. 运行验证"
    ;;

  2)
    TARGET="crates/aero-storage/src/message.rs"
    TARGET_DIR="crates/aero-storage/src/message"
    LINES=$(wc -l < "$TARGET")
    echo "Step 2: 拆分 $TARGET ($LINES 行) → $TARGET_DIR/"
    echo ""
    echo "  预计拆分为:"
    echo "    message/mod.rs      — pub use 重新导出"
    echo "    message/repo.rs     — insert, get, list, update, delete_soft"
    echo "    message/search.rs   — FTS, vector, hybrid 搜索"
    echo "    message/retention.rs — sweep_expired, sweep_ephemeral"
    echo ""
    echo "  操作指引同上。"
    ;;

  3)
    TARGET="crates/aero-storage/src/workspace.rs"
    TARGET_DIR="crates/aero-storage/src/workspace"
    LINES=$(wc -l < "$TARGET")
    echo "Step 3: 拆分 $TARGET ($LINES 行) → $TARGET_DIR/"
    echo ""
    echo "  预计拆分为:"
    echo "    workspace/mod.rs     — pub use + Workspace struct"
    echo "    workspace/repo.rs    — CRUD workspace"
    echo "    workspace/members.rs — member_role, add_member, remove_member"
    echo "    workspace/settings.rs — settings, SSO, SCIM 配置"
    ;;

  4)
    TARGET="crates/aero-ai/src/service.rs"
    TARGET_DIR="crates/aero-ai/src/service"
    LINES=$(wc -l < "$TARGET")
    echo "Step 4: 拆分 $TARGET ($LINES 行) → $TARGET_DIR/"
    echo ""
    echo "  预计拆分为:"
    echo "    service/mod.rs      — pub use + AiService struct"
    echo "    service/summarize.rs — summarize_room, summarize_thread"
    echo "    service/answer.rs   — answer_question, workspace_ask"
    echo "    service/embed.rs    — embed_text, embed_messages"
    echo "    service/moderate.rs — moderate_content"
    ;;

  5)
    TARGET="crates/aero-server/src/ws.rs"
    TARGET_DIR="crates/aero-server/src/ws"
    LINES=$(wc -l < "$TARGET")
    echo "Step 5: 拆分 $TARGET ($LINES 行) → $TARGET_DIR/"
    echo ""
    echo "  预计拆分为:"
    echo "    ws/mod.rs           — pub use + ws_handler"
    echo "    ws/rooms.rs         — join_room, send_message, edit_message, ..."
    echo "    ws/calls.rs         — call_invite, call_answer, call_ice, call_end"
    echo "    ws/streams.rs       — watch_stream, stream_chat, stream_gift"
    ;;

  6)
    TARGET="web/app.js"
    echo "Step 6: 拆分 $TARGET (2056 行) → web/views/"
    echo ""
    echo "  预计拆分为:"
    echo "    app.js              — 保留引导和路由（≤ 200 行）"
    echo "    views/auth.js       — 登录/注册视图"
    echo "    views/room.js       — 聊天房间视图"
    echo "    views/stream.js     — 直播视图"
    echo "    views/call.js       — 通话视图"
    echo "    views/settings.js   — 设置视图"
    ;;

  7)
    TARGET="crates/aero-common/src/model.rs"
    TARGET_DIR="crates/aero-common/src/model"
    LINES=$(wc -l < "$TARGET")
    echo "Step 7: 拆分 $TARGET ($LINES 行) → $TARGET_DIR/"
    echo ""
    echo "  警告：model.rs 被所有 crate 引用，拆分需谨慎"
    echo "  预计拆分为:"
    echo "    model/mod.rs        — pub use 重新导出"
    echo "    model/room.rs       — Room, RoomKind, RoomEvent"
    echo "    model/message.rs    — Message, MessageEnvelope, Block"
    echo "    model/participant.rs — Participant, ParticipantKind"
    echo "    model/call.rs       — CallEvent, CallId, CallKind"
    echo "    model/reaction.rs   — Reaction, ReactionSummary"
    ;;

  *)
    echo "未知 Step: $STEP"
    echo "可用: 1-7（对应 docs/REFACTOR_PLAN.md）"
    exit 1
    ;;
esac

echo ""
echo "────────────────────────────────────────────"
echo "验证步骤（拆分后必须执行）："
echo ""
echo "  # 1. 编译检查"
echo "  cargo check --workspace --quiet"
echo ""
echo "  # 2. 测试"
echo "  cargo test --workspace --lib --quiet"
echo ""
echo "  # 3. 文件尺寸（rebase 模式——只检查新增违规）"
echo "  make check-rebase"
echo ""
echo "  # 4. 更新基线"
echo "  make init-baseline"
echo ""
echo "  # 5. 完整检查"
echo "  make check-full"
