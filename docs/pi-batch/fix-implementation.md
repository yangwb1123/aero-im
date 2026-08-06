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
