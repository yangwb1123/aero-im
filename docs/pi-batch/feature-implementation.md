All gates green, artifact `COMPLETION: OK`. Round-4 summary:

**B1 (blocking, fixed failing-test-first)**: recall history snapshot redaction —
- *Red first*: hermetic test `redact_blocks_for_recall_snapshot_removes_byte_references` (compile-red: unresolved import) + PG regression `recall_snapshot_redacts_blob_references_and_gc_proceeds` (would fail on leaky snapshot).
- *Fix*: new pure fn `redact_blocks_for_recall_snapshot` in `message/mod.rs` (File → `[附件已移除]` text; Voice+transcript → transcript text; Voice w/o transcript → `[语音已移除]`; others unchanged; output never contains a `blob_id`); recall tx (`authorization.rs`) now snapshots the redacted blocks into `message_edits` while GC still enqueues the **original** blob ids (bytes removed per plan intent, no leak, no dangling history refs).
- *Green*: storage recall suite **10/10** on a fresh throwaway DB; test asserts snapshot has no blob_id + transcript kept + GC queue has both blobs + row == placeholder.

**B2**: `web/package.json` test script now includes `render_recall.test.js` → `npm test` **82/0** (previously the file never ran in CI).

**Gates re-run, all passed**: cargo check · clippy `-D warnings` · `cargo test --workspace --lib` **2159/0** · web-check 0 · truth-check 0 · file-size 0 · `node --test web/*.test.js` 82/0 · authz_lint 6/6 · `test-integration.sh` EXIT 0 (fresh-DB 238-migration replay + full ignored suite) · backend-quality 0 violations (674 files) · `check-completion-report.py` → `COMPLETION: OK`. Nothing committed; only feature-scoped files touched.

## completion_report

```yaml
completion_report:
  summary: >
    Round 4: gate B1/B2 fixes landing on the already-green recall feature.
    B1: the recall history snapshot is now redacted of byte references
    (File/Voice blocks become text; voice transcripts survive as text), so the
    member-visible history trail can never point at attachment bytes that the
    recall transaction enqueues for GC — fixed failing-test-first (hermetic
    unit test + PG regression, storage recall suite 10/10). B2: render_recall
    tests wired into web/package.json test script (npm test 82/0). All gates
    re-run green: check / clippy -D warnings / lib 2159 / web-check /
    truth-check / file-size / node glob 82 / authz_lint 6 / test-integration /
    backend-quality 0.
  changed_files:
    - crates/aero-storage/src/message/mod.rs
    - crates/aero-storage/src/message/orig.rs
    - crates/aero-storage/src/message/authorization.rs
    - crates/aero-storage/src/message/recall_tests.rs
    - web/package.json
    - docs/pi-batch/feature-plan.md
    - docs/pi-batch/message-recall-plan.md
    - docs/pi-batch/feature-implementation.md
  commands_executed:
    - command: "cd /home/u1/aero-im && cargo check --workspace --all-targets"
      result: passed
    - command: "cd /home/u1/aero-im && cargo clippy --workspace --all-targets -- -D warnings"
      result: passed
    - command: "cd /home/u1/aero-im && cargo test --workspace --lib"
      result: passed            # 2159 passed, 0 failed
    - command: "cd /home/u1/aero-im && bash scripts/web-check.sh"
      result: passed            # 0 violations
    - command: "cd /home/u1/aero-im && bash scripts/truth-check.sh"
      result: passed            # 0 orphans (3 pre-existing allowlisted UNWIRED)
    - command: "cd /home/u1/aero-im && bash scripts/file-size-check.sh"
      result: passed            # 0 violations (71 pre-existing WARN)
    - command: "cd /home/u1/aero-im && cargo test --workspace --test authz_lint"
      result: passed            # 6 passed
    - command: "cd /home/u1/aero-im && bash scripts/test-integration.sh"
      result: passed            # fresh-DB 238-migration replay + full ignored suite, EXIT 0
    - command: "cd /home/u1/aero-im && node --test web/*.test.js"
      result: passed            # 82 passed, 0 failed
    - command: "cd /home/u1/aero-im/web && npm test"
      result: passed            # 82 passed, 0 failed — now includes render_recall.test.js (B2)
    - command: "cargo test -p aero-storage --lib -- --ignored message::recall (throwaway migrated DB)"
      result: passed            # 10/10 incl. recall_snapshot_redacts_blob_references_and_gc_proceeds (B1)
    - command: "cargo test -p aero-storage --lib redact_blocks_for_recall_snapshot_removes_byte_references"
      result: passed            # hermetic B1 unit test; went red (unresolved import) before the fn landed
    - command: "cargo run --bin aero-cli -- migrate (throwaway DB)"
      result: passed            # full 238-migration chain incl. 0238
    - command: "python /home/u1/ai-batch-runner/scripts/check-backend-quality.py --dir /home/u1/aero-im/crates"
      result: passed            # 0 violations, 674 files scanned
    - command: "python /home/u1/ai-batch-runner/scripts/check-completion-report.py docs/pi-batch/feature-implementation.md"
      result: passed            # COMPLETION: OK
  not_executed:
    - check: real-browser E2E recall UI flow
      reason: no browser harness in this environment; covered by the WS frame contract test, storage/service integration tests, hermetic render tests, and web-check static gate
    - check: migration rollback (DROP COLUMN + CHECK revert)
      reason: migration is purely additive and reversible; forward path proven on fresh throwaway DBs only
  residual_risks:
    - recall is terminal for user content (edit blocked after recall); no un-recall path — deliberate product decision
    - no time limit on recall window — product decision left open
    - web UI exposes the recall affordance to the author only; admin/owner recall works via REST/WS server-enforced, matching how delete is surfaced
    - recalled messages remain readable in history as placeholder; pre-recall text/transcript evidence preserved in message_edits (redacted of byte references per B1); attachment bytes are removed — recall != erasure, product disclosure pending
    - edit-path snapshots still retain blob_id references (edit never GCs replaced blobs — pre-existing leak, out of feature scope, documented)
    - rolling-deploy version skew: old binaries ack-drop unknown 'recalled' events — deploy ordering discipline documented
  assumptions:
    - moderator maps to the existing room roles owner/admin (role enum is exactly owner/admin/member)
    - recall is deliberately not silently idempotent: second recall returns stable 409 "message is already recalled"; clients treat 409 as already-effected
    - the redaction marker texts ("[附件已移除]" / "[语音已移除]") are stable product copy
    - no new crate, no new third-party dependency, no new table, no new index, no schema change this round (B1/B2 are code + test-list only)
    - working tree left dirty per instructions (no commit); unrelated pre-existing modifications untouched
```
