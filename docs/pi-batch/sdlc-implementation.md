# Recall Window (撤回时间窗) — Implementation Report

Feature: configurable recall time window (`AERO_RECALL_WINDOW_SECS`, default
86400s; 0 = unlimited). Authors may recall only within the window; room
owner/admin recall is exempt (moderation path). Boundary enforced atomically
in the row-locked transaction.

## completion_report

```yaml
completion_report:
  summary: >
    recall_window_expired(created_at, now, window) shared inclusive-boundary
    predicate (age <= window allowed; 0 = unlimited); config parsing
    (0/unset/garbage/negative/overflow -> default); window checked in
    assert_message_recall_preflight (after role gate, before rate charge)
    and re-checked row-locked in recall_outboxed_authorized (author-only
    path, FOR UPDATE snapshot); admin override exempt; metric
    aero_messages_recall_expired_total; web recallErrorToast whitelist
    (window-expired info toast, other 409 error toast); docs in
    .env.example, README env table + feature matrix, docs/recall-window.md.
  changed_files:
    - crates/aero-common/src/model/message.rs
    - crates/aero-common/src/lib.rs
    - crates/aero-common/src/metrics.rs
    - crates/aero-im-core/src/service/messages.rs
    - crates/aero-im-core/src/service/orig.rs
    - crates/aero-im-core/src/db_tests.rs
    - crates/aero-im-core/src/db_tests/recall_tests.rs
    - crates/aero-storage/src/message/authorization.rs
    - crates/aero-storage/src/message/recall_tests.rs
    - crates/aero-storage/src/message/recall_index_fence_tests.rs
    - crates/aero-server/src/error.rs
    - web/recall_errors.js
    - web/recall_errors.test.js
    - web/app.js
    - web/api.test.js
    - .env.example
    - README.md
    - docs/recall-window.md
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
    - command: "node web/recall_errors.test.js"
      result: passed
  not_executed:
    - check: real-browser recall-window E2E
      reason: no browser harness; web error-toast behavior covered by
        recall_errors.test.js (6 cases incl. whitelist pin)
  residual_risks:
    - clock skew between app instances changes the effective boundary by
      the skew amount (documented in docs/recall-window.md)
    - window is per-created_at, not per-event; a delayed outbox replay
      after the window still applies (recall happened in-window)
  assumptions:
    - admin override is unconditional (no window) — moderation path
    - 0 = unlimited is the explicit opt-out; unset/garbage falls back to
      the 24h default
```

## 验收对照（requirements → evidence）

| 验收标准 | 证据 |
|---|---|
| t = window 允许、t = window+1s 过期 | `service/messages.rs` 边界单测（单次捕获 now）+ `recall_tests.rs` margins 86399/86401 |
| admin 不受窗口限制 | `recall_tests.rs` admin override 测试 + 存储层 role 分支 |
| 配置 0/unset/负数/溢出 → 默认 | `parse_recall_window` 表驱动单测 |
| 事务内原子评估（非仅 preflight） | `recall_outboxed_authorized` FOR UPDATE 快照上的 author-only 重查（authorization.rs） |
| 409 契约 | `error.rs` contract 测试（thiserror 前缀 + 409）；web whitelist 只对两个收敛字面量静默 |
| 文档 | .env.example:149、README env 表 + 功能矩阵、docs/recall-window.md |
