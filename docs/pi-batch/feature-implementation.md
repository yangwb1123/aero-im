# Message Recall (消息撤回) — Implementation Report

Feature: message recall — author or room owner/admin recalls a message; content
is replaced with the system placeholder (row/history/audit intact), a `Recalled`
room event is broadcast, all clients render the placeholder. Multi-tenant
boundaries enforced (no cross-room/workspace oracle).

## completion_report

```yaml
completion_report:
  summary: >
    Message recall implemented end to end: REST + WS entry, outbox
    transactional recall, Recalled room event broadcast, client
    placeholder rendering, migration 0238, permission matrix tests.
  changed_files:
    - migrations/0238_message_recall.sql
    - crates/aero-common/src/model/message.rs
    - crates/aero-common/src/model/event.rs
    - crates/aero-common/src/model/tests.rs
    - crates/aero-common/src/lib.rs
    - crates/aero-common/src/metrics.rs
    - crates/aero-im-core/src/events.rs
    - crates/aero-im-core/src/service/messages.rs
    - crates/aero-im-core/src/service/outbox.rs
    - crates/aero-im-core/src/db_tests.rs
    - crates/aero-server/src/routes/handlers/messages.rs
    - crates/aero-server/src/routes/routes.rs
    - crates/aero-server/src/bin/boot/background.rs
    - crates/aero-server/src/broadcast.rs
    - crates/aero-server/src/bot_dispatch.rs
    - crates/aero-server/src/bot_dispatch/tests.rs
    - crates/aero-server/src/conversation_export.rs
    - crates/aero-server/src/forward.rs
    - crates/aero-server/src/message_context.rs
    - crates/aero-server/src/ooo_bot.rs
    - crates/aero-server/src/unfurl_bot.rs
    - crates/aero-server/src/webhooks.rs
    - crates/aero-ai/src/rerank.rs
    - crates/aero-ai/src/service/profile.rs
    - crates/aero-ai/src/service/tests.rs
    - web/render.js
    - web/style.css
  commands_executed:
    - command: "cargo check --workspace --all-targets"
      result: passed
    - command: "cargo clippy --workspace --all-targets -- -D warnings"
      result: passed
    - command: "cargo test --workspace --lib"
      result: passed
    - command: "bash scripts/web-check.sh"
      result: passed
    - command: "bash scripts/truth-check.sh"
      result: passed
    - command: "bash scripts/test-integration.sh"
      result: passed
  not_executed:
    - check: real-browser E2E recall UI flow
      reason: no browser harness in this environment; client rendering covered by
        render.js placeholder path + web-check import/syntax gate
  residual_risks:
    - recall is terminal for user content (edit blocked after recall); no
      un-recall path — deliberate, matches Slack-style placeholder semantics
    - no time limit on recall window — product decision left open (see
      docs/pi-batch/feature-plan.md)
  assumptions:
    - recalled placeholder body is immutable; moderation delete still applies
    - WS frame name `recall_message` mirrors REST route; both share the same
      service method (single invariant)
```

## 设计要点（与 plan 对照）

- **权限矩阵**（纯函数 `recall_authorized`，表驱动单测）：作者放行；owner/admin
  放行；普通成员非作者 403；非成员 403（访问守卫先于状态检查，无存在性 oracle）。
- **稳定失败路径**（求值顺序）：404 未知消息 → 403 无房间访问 → 409 已删除 →
  409 已撤回 → 403 无权。存储事务在行锁下重查访问/角色/状态（预检只是 UX）。
- **持久化**（迁移 0238）：`recalled_at` / `recalled_by` 列 + 占位 body 写入，
  行/历史/审计完整保留；`recall_outboxed_authorized` 与 event_outbox 同事务，
  快速 dispatch 失败回落 durable relay（与既有编辑/删除同一不变量）。
- **事件**：`Recalled` room event 经 `im.room.{id}` 广播，客户端 render.js
  渲染占位（"此消息已被撤回"）。
- **指标**：`aero_messages_recalled_total` + 处理耗时直方图（op=recall）。
