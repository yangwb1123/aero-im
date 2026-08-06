# Recall Security Fixes (F1/F3) + Test Isolation — Implementation Report

Round 6: address ai/run-review.py stage 02/06 findings on the recall feature
(F1 index-write resurrection, F3 missing rate gates), plus fix order-dependent
integration tests exposed by the campaign's over-broad `--skip db_tests::`.

## completion_report

```yaml
completion_report:
  summary: >
    F1: lockless index UPDATEs (update_embedding, update_searchable_text,
    update_voice_transcript) now fence `recalled_at IS NULL` in their WHERE
    clause (crud.rs 449/475/506); deterministic race tests in
    message/recall_index_fence_tests.rs prove a pre-recall worker write is a
    no-op post-recall. F3: recall now passes the same rate gates as
    send/edit — check_ws_rate_room on the WS RecallMessage frame and the
    REST handler path, plus a tenant-fairness preflight. Test isolation:
    call/security_tests.rs + dm/block_tests.rs cleanup drops workspace
    memberships before raw participant DELETE (0227 owner guard);
    test-integration.sh skip narrowed from `db_tests::` to the two new
    aero-im-core modules so the aero-storage batch composition (nil
    workspace state machine) is restored.
  changed_files:
    - crates/aero-storage/src/message/crud.rs
    - crates/aero-storage/src/message/recall_index_fence_tests.rs
    - crates/aero-storage/src/message/mod.rs
    - crates/aero-storage/src/message/query.rs
    - crates/aero-storage/src/message/recall_tests.rs
    - crates/aero-im-core/src/service/messages.rs
    - crates/aero-im-core/src/db_tests/recall_tests.rs
    - crates/aero-server/src/routes/handlers/messages.rs
    - crates/aero-server/src/ws/ws_impl/frame.rs
    - crates/aero-server/tests/authz_lint.rs
    - crates/aero-storage/src/call/security_tests.rs
    - crates/aero-storage/src/dm/block_tests.rs
    - scripts/test-integration.sh
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
    - check: real-provider AI worker E2E (embedding race under live traffic)
      reason: needs a real provider round-trip; the DB-level race is covered
        by recall_index_fence_tests (recall-then-write deterministic repro)
  residual_risks:
    - system-edit paths (edit_outboxed_system / transcript) rely on row
      lock + re-check rather than a SQL WHERE fence; the lock serializes
      with recall's UPDATE so the read snapshot cannot go stale
    - the three order-dependent tests now delete memberships first, but
      their nil-workspace bootstrap still assumes batch composition
  assumptions:
    - recall remains terminal for user content; index writes after recall
      are no-ops (returns false), callers treat them as benign
```

## 评审发现 → 修复对照（ai/run-review.py stage 02/06）

| 发现 | 修复 |
|---|---|
| F1 (Medium) update_embedding/update_searchable_text 无 recalled_at 围栏，撤回内容可经 FTS/向量搜索复活 | crud.rs 三处无锁索引 UPDATE 全部加 `AND recalled_at IS NULL`；新增 recall_index_fence_tests.rs（确定性复现：先撤回后写入必须 no-op，索引清除状态存活） |
| F3 (Medium) 撤回绕过 send/edit 的限流 | WS RecallMessage 帧走 `check_ws_rate_room`；REST 处理器与编辑同路径限流；新增租户公平性 preflight（`assert_message_recall_preflight` 先解析房间再做重活） |
| stage 06 H2-H3/M2/M3（可观测性/迁移并发安全/双客户端 E2E） | 记录为运营验收项（docs/DECISIONS.md），非代码阻断 |
| 集成套件顺序依赖暴露（campaign 的 `--skip db_tests::` 过宽） | skip 精确到 `db_tests::notifications_tests` + `db_tests::relay_tests`；call/dm 测试清理先删成员再删参与者（0227 guard 兼容） |

## 调试记录（本次事故 → learn 素材）

`racing_block_completes_before_direct_call_can_start` 基线通过、当前失败：
worktree 基线复现 + stash 隔离 + 串行/并行对比，最终定位为 `--skip db_tests::`
子串过宽（`ai_job::db_tests` 等 aero-storage 测试被连带跳过，破坏 nil
workspace 状态机）。教训：`--skip` 过滤必须精确到模块路径，禁止用短子串
匹配跨 crate 的测试名（已沉淀 docs/rules/drafts 建议）。

---

## Round 2 — Gate S1 (rate-gate DoS amplifier) + non-blocking findings

The gate review (fix-gate.md) FAILED round 1 on the security engineer's S1 /
async reviewer's Finding 1: `assert_message_recall_preflight` checked room
access + deleted + recalled but NOT `recall_authorized`, and both entry points
charged `check_ws_rate_room` between the preflight and the role gate — so a
non-author room member could drain the shared per-workspace budget (default
1200/min, shared with send/edit/history/search) with doomed recall attempts
over an unthrottled WS socket. Closed in this round, plus the Low findings.

### completion_report

```yaml
completion_report:
  summary: >
    S1: the recall role gate (author or room owner/admin) is hoisted INSIDE
    assert_message_recall_preflight (messages.rs), so a doomed recall by a
    plain member is Forbidden BEFORE any edge can charge the shared workspace
    budget — the F3 gate is no longer a DoS amplifier and now truly mirrors
    edit's preflight shape (edit carries its sender gate). Preflight test
    extended (member -> Forbidden with the exact message; promoted admin ->
    resolves the room); mutation-proven non-vacuous (removing the gate makes
    the test fail at the member assertion). authz_lint F3 scanner hardened to
    require preflight -> check_ws_rate_room -> recall_message ordering.
    Behavioral proof: scripts/smoke_recall_rate_gate.py against a live server
    with AERO_WS_RATE_STANDARD_PER_MIN=3 — 5 doomed member recalls all 403
    with zero budget consumed (owner's own recall then succeeds), owner's next
    recall 429 + Retry-After, WS error frame code=rate_limited. S2: REST 429s
    now carry Retry-After (ApiError fixed-window hint, per-client middleware
    still overrides with its exact value); WS error frames propagate the
    stable error code instead of the generic "handler"; web client maps 429 /
    rate_limited to a back-off toast. Medium test gaps: concurrent race tests
    for the two transactional outboxed writers (edit_outboxed_system +
    update_voice_transcript_outboxed, 8 rounds each, final-state invariants
    hold under any interleaving). transcribe_bot doc drift fixed.
  changed_files:
    - crates/aero-im-core/src/service/messages.rs      # role gate in preflight
    - crates/aero-im-core/src/db_tests/recall_tests.rs # member/admin preflight asserts
    - crates/aero-server/tests/authz_lint.rs           # S1 ordering pin
    - crates/aero-server/src/error.rs                  # Retry-After on 429
    - crates/aero-server/src/ws/ws_impl/mod.rs         # stable WS error codes
    - crates/aero-server/src/transcribe_bot.rs         # doc drift
    - crates/aero-storage/src/message/recall_index_fence_tests.rs # 2 race tests
    - web/app.js                                       # 429/rate_limited mapping
    - scripts/smoke_recall_rate_gate.py                # behavioral gate smoke
  commands_executed:
    - command: "cargo check --workspace --all-targets"
      result: passed
    - command: "cargo clippy --workspace --all-targets -- -D warnings"
      result: passed
    - command: "cargo test --workspace --lib"
      result: "passed (319 passed / 606 ignored)"
    - command: "cargo test -p aero-server --test authz_lint"
      result: "passed (7/7 incl. every_recall_entry_point_charges_ws_rate_budget)"
    - command: "storage recall suite on fresh throwaway DB (aero_review_s1, 238 migrations)"
      result: "passed 14/14 incl. both new outboxed-writer race tests"
    - command: "im-core recall suite on same DB"
      result: "passed 4/4 incl. recall_preflight_resolves_room_and_early_errors"
    - command: "mutation test: role gate removed from preflight"
      result: "preflight test FAILED at the plain-member assertion (non-vacuous); restored"
    - command: "bash scripts/test-integration.sh"
      result: "passed (599 ignored tests on throwaway DBs, DBs dropped)"
    - command: "bash scripts/web-check.sh / truth-check.sh / file-size-check.sh"
      result: "passed (0 violations; 0 ORPHAN; app.js 998 lines)"
    - command: "scripts/smoke_recall_rate_gate.py against live server (AERO_WS_RATE_STANDARD_PER_MIN=3)"
      result: "passed — conservation proven, REST 429 + Retry-After, WS rate_limited frame"
  not_executed:
    - check: none — all gates including the behavioral smoke ran
  residual_risks:
    - the live smoke depends on the server being booted with
      AERO_WS_RATE_STANDARD_PER_MIN=3 (documented in the script header); not
      part of the hermetic CI gates, matching the smoke harness convention
    - system-edit/transcript writers remain row-lock + Rust re-check (no SQL
      WHERE fence); now covered by concurrent race tests under any
      interleaving
