Plan v4 complete — both documents updated, no implementation code written.

## What this round produced

**1. Assessment** — same prescription: demo(S) / L3_platform / L2_platform_capability, 0 hard rules, 多租户 signal.

**2. Gate round-3 verdict incorporated (2 findings, B1 blocking)** — I verified the B1 mechanics directly in source before designing the fix:
- **B1 (MED/HIGH, recall-specific)**: recall tx snapshots original blocks (incl. `File`/`Voice` `blob_id`s) into `message_edits` then enqueues those same blobs for GC; `enqueue_unreferenced_blobs_in_tx` + drain-time `has_live_references` scan only `messages` — never `message_edits` — while the member-gated history route serves the snapshot for live-recalled rows. Bytes die in ~60s; history points at dead refs. I confirmed it's unique to recall (delete snapshots nothing; edit snapshots but never GCs).
- **Fix chosen: Option A (snapshot redaction)** — new pure fn `redact_blocks_for_recall_snapshot` (`File` → `[附件已移除]` text block; `Voice{transcript}` → text block with transcript; `Voice{None}` → `[语音已移除]`; others unchanged; output never contains a `blob_id`), used for the `message_edits` insert while GC still enqueues the original bytes (no leak, no dangling refs). **Option B rejected with a decision record**: `message_edits` is retained on tombstone (crud.rs:539), so scanning it would permanently disable delete-path GC and add hot-path scans of an append-only evidence table.
- **B2 (LOW)**: `web/package.json` `"test"` omits `render_recall.test.js` while CI runs `npm test` → append to the list.

**3. Failing-test-first recipes (§7.4)** — hermetic redaction-fn unit test (red: fn doesn't exist), PG regression `recall_snapshot_redacts_blob_references_and_gc_proceeds` (red today: snapshot contains blob_ids; asserts snapshot has no blob_id + transcript kept + GC queue still has both blobs + row == placeholder), and the `npm test` wiring check.

**4. Plan updates** — `message-recall-plan.md` v4 (§0.5 findings table, §2 Snapshot Fields redaction + decision record, §5.1/5.3/5.5 tests, §6.1 change radius incl. `mod.rs`/`authorization.rs`/`package.json`, §7.4, §8 DoD with storage recall **10/10** + `npm test`); `feature-plan.md` rewritten for the implementer with the checklist, explicit not-in-scope list (gate-dismissed findings), and the §8.5 artifact contract (YAML `completion_report` block + `check-completion-report.py` → `COMPLETION: OK` — the previous VALIDATION_FAILED root cause, still mandatory).

**DoD for the next round**: both B1 regression tests green (storage recall 10/10), `cd web && npm test` green with `render_recall.test.js` included, all gates re-run (check/clippy/lib/web-check/truth-check/file-size/node/authz_lint/test-integration/backend-quality), and the artifact passes the completion validator.
