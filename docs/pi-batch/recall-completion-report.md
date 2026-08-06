# Completion Report — Message Recall (消息撤回)

## Implementer Round (plan v2 §7.2 gaps closed; all gates re-run clean)

Closed the three test gaps the plan identified, failing-test-first where the
behavior was not already pinned, and re-ran every gate:

1. **Gap ① — `materialize_outbox_payload` Recalled arm unit test**
   (`aero-im-core/src/service/outbox.rs`): `recalled_payload_is_delivered_at_version_and_suppressed_when_superseded`
   pins all four branches — delivered with current placeholder blocks at the
   matching version; suppressed when a later mutation bumped the version;
   suppressed when tombstoned; suppressed when the row is gone.
2. **Gap ② — concurrent double-recall race**
   (`aero-storage/src/message/recall_tests.rs`
   `concurrent_double_recall_has_exactly_one_winner`): `tokio::join!` two
   `recall_outboxed_authorized` calls → exactly one `Ok(Some)`, one stable
   `Conflict("message is already recalled")`, placeholder body, exactly one
   version bump, exactly one `recalled` outbox row. (First compile of this
   test failed on a moved-value error — fixed to borrow; then green.)
3. **Gap ③ — web recall tests** (previously zero recall coverage):
   - `web/ws.test.js`: `recallMessage` sends `{"type":"recall_message","id"}`;
     `mutation frames never advance the legacy backfill cursor`.
   - `web/api.test.js`: `recallMessage` POSTs to the path-encoded
     `/api/messages/:id/recall` with no body; 409 already-recalled surfaces as
     `ApiError(status=409, code=conflict)` for the caller's 409→success map.
   - `web/render_recall.test.js` (new): recalled message renders placeholder
     + `· 已撤回` badge + `.recalled` class with **zero** action buttons
     (terminal), not a tombstone; recall affordance visible to the author
     only.
4. **Client-side defect found by the new test (fixed failing-test-first):**
   `web/ws.js` advanced the legacy `?since=` cursor on ANY applied frame with
   a `message.id` — including `recalled`/`edited`/`deleted` — so a mutation
   frame for a message whose create was never fetched could skip that create
   in the legacy backfill for the rest of the session (async-reviewer #7).
   The test reproduced it red; fixed by restricting `_lastSeen` advancement
   to `msg.type === 'message'` frames (mirroring `_queueDeliveryAck`'s
   existing policy). Mutations converge via `changes_since`, never the legacy
   new-message cursor.

All five gate defects (P1 system-edit fence, P1/P2 replay predicate + index,
P2 client dedupe funnel, P2 SPA entry point, HIGH partition backfill) were
re-verified present-and-fixed in the tree, and their regression tests all pass.

## Gate-Fix Round (post-review; all five blocking findings resolved + regression-tested)

1. **P1 content resurrection via system edit** — `edit_locked_outboxed_in_tx` and
   `update_voice_transcript_outboxed` (storage `message/events.rs`) now fence
   `recalled_at` alongside `deleted_at`: a late unfurl/transcribe edit returns
   `None` and can never rewrite the placeholder with the original body. Test:
   `system_edit_after_recall_is_fenced`.
2. **P2 change-replay never delivers recalls** — `changes_since` now keys on
   `GREATEST(edited_at, deleted_at, recalled_at)` (WHERE + ORDER BY), and
   migration 0238 reissues `idx_messages_room_mutated` with that expression
   (DROP+CREATE; the index is not a constraint). Test:
   `changes_since_delivers_recalls`.
3. **P2 client dedupe drops the placeholder** — `handleIncomingMessage` routes a
   held-id row carrying `edited_at`/`recalled_at`/`deleted_at` (reconnect
   backfill) through `applyChange` (the same out-of-order/resurrect-guarded
   funnel as live events) instead of dropping it; identical redeliveries are
   still deduped.
4. **P2 SPA receive-only** — added `ws.recallMessage(id)` (`recall_message`
   frame), `api.recallMessage(id)` (`POST /api/messages/:id/recall`), a
   `↶` recall button on self messages (render.js), and the `wireMsgActions`
   handler with 409→success mapping (`ApiError.status === 409` = already
   recalled/deleted, which is the desired end state; `msg:recalled` broadcast
   or replay converges the row).
5. **HIGH partition-cutover backfill omits recall columns** — migration 0238
   reissues `backfill_messages_partition` with `recalled_at`/`recalled_by` in
   the explicit column list (0148/0149/0158/0174 convention). Test:
   `partition_backfill_carries_recall_columns` (scoped `from_id` cursor so the
   batch covers only the fixture message — a whole-table sweep trips the
   shadow's blob-workspace-scope trigger on rows the blob fence tests
   deliberately leave cross-scoped).

All six repo gates re-verified after the fix round: `cargo check --workspace
--all-targets` ✓, `cargo clippy --workspace --all-targets -- -D warnings` ✓,
`cargo test --workspace --lib` (2157 passed / 0 failed) ✓, `bash
scripts/test-integration.sh` (593 ignored tests on throwaway DBs, incl.
fresh-DB 238-migration replay) ✓, `scripts/{web-check,truth-check,file-size-check}.sh`
0 violations ✓, web `node --test` 76/0 ✓.

## Summary

Implemented message recall end-to-end in the Aero IM workspace: sender (or room owner/admin) recalls a message via `POST /api/messages/:id/recall` or WS `recall_message`; the content is replaced by the system placeholder `[此消息已被撤回]` in one transaction (placeholder blocks + `searchable_text=''` + `embedding=NULL` + `recalled_at`/`recalled_by` + `version+1`), pre-recall body snapshotted into `message_edits`, `message.recalled` audit row appended, attachment blobs enqueued for GC, and a durable `RoomEvent::Recalled(Message)` → `{"type":"recalled","message":{...}}` WS frame fanned out to all room members so every client renders the placeholder in place. Multi-tenant boundary enforced via `assert_room_access` (participant-first) + commit-time fenced re-check under row locks; stable errors 404/403/409 with fixed check ordering (no cross-tenant existence oracle). Migration `0238_message_recall.sql` is purely additive (2 nullable columns + `messages_partitioned` mirror + `event_outbox_kind_check` extension); verified by fresh-DB replay in `scripts/test-integration.sh`.

## Changed Files

**New:**
- `migrations/0238_message_recall.sql` — recall columns + shadow mirror + outbox kind CHECK
- `crates/aero-storage/src/message/recall_tests.rs` — 5 PG-gated integration tests (tx contents, permission matrix, cross-tenant isolation, delete-after-recall, schema/migration assertions)
- `crates/aero-im-core/src/db_tests/recall_tests.rs` — 3 PG-gated service tests (stable error ordering, admin path, room-boundary isolation)
- `docs/pi-batch/message-recall-plan.md` — the plan (already in tree from planning stage)

**Core implementation:**
- `crates/aero-common/src/model/message.rs` — `Message.recalled_at`/`recalled_by` + `recalled()` + `RECALLED_MESSAGE_PLACEHOLDER`; `lib.rs` re-export; `model/event.rs` — `RoomEvent::Recalled(Message)` + accessors
- `crates/aero-storage/src/message/{authorization.rs,events.rs}` — `recall_outboxed_authorized` + `recall_locked_outboxed_in_tx` + in-tx `recall_role_allowed_in_tx` (FOR UPDATE role re-check)
- `crates/aero-storage/src/message/mod.rs` — `MessageRow`/`ScoredMessageRow` + From impls; `query.rs`/`crud.rs`/`thread.rs`/`search.rs`/`search_query.rs`/`bookmark.rs`/`pin.rs`/`workspace/export.rs`/`message/idempotency.rs` — SELECT column lists
- `crates/aero-storage/src/event_outbox.rs` — `EventOutboxKind::Recalled`
- `crates/aero-im-core/src/service/messages.rs` — `ImService::recall_message` + pure `recall_authorized` + `editable_message` rejects recalled; `service/outbox.rs` — `materialize_outbox_payload` Recalled arm
- `crates/aero-server/src/ws/ws_impl/{mod.rs,frame.rs}` — `ClientFrame::RecallMessage` + `ServerFrame::Recalled`; `ws/frame.rs` — frame serialization arm; `ws/ws_impl/bus.rs` — AI answer-cache invalidation covers Recalled; `routes/handlers/messages.rs` + `routes/routes.rs` — REST endpoint; `bot_dispatch.rs`/`webhooks.rs` — event-kind strings
- `web/{app.js,render.js,context.js,style.css}` — `msg:recalled` handler (shared out-of-order/resurrect guards), `isRecalled` render + suppressed actions, CSS
- `crates/aero-common/src/metrics.rs` — `MESSAGES_RECALLED_TOTAL`
- Mechanical consequences: `ImEvent::MessageSent` boxed (wire-transparent; clippy large_enum_variant after Message grew), `Box::pin` in `ooo_bot.rs`/`unfurl_bot.rs`/`background.rs` (large_futures), Message-literal test fixtures in aero-ai/aero-server/aero-storage

## Requirements Covered

- Sender can recall own message ✅ (author path; service + storage tests)
- Content replaced by system placeholder, message_id/room history/audit kept ✅ (row survives, `message_edits` snapshot, audit row; asserted in `author_recall_replaces_content_and_records_audit_in_one_tx`)
- `recalled_by` + `recalled_at` recorded ✅
- WS event broadcast, all clients render placeholder ✅ (`RoomEvent::Recalled` → `type:"recalled"` frame; web `handleRecalled` renders placeholder; contract test `recalled_frame_shape_carries_placeholder_message`)
- Only author or room admin/moderator; non-author 403 ✅ (room roles owner/admin; matrix tests at pure-fn, storage, and service levels)
- Failure paths stable: unknown message 404 / already-recalled 409 / not-a-member 403 / no-permission 403 ✅ (fixed ordering, no state oracle)
- Multi-tenant boundary not crossable ✅ (storage + service cross-workspace/cross-room tests)
- Acceptance: permission matrix unit tests ✅, DB test for migration ✅ (`recall_schema_columns_and_outbox_kind_are_applied` + fresh-DB replay), WS event shape verified ✅ (contract test)

## Tests Added

- Unit (hermetic): `recall_permission_matrix` (im-core), `recalled_event_roundtrips_and_fans_to_room`, `message_deserializes_without_recall_fields`, `recalled_placeholder_is_single_text_block` (aero-common), `recalled_frame_shape_carries_placeholder_message` (aero-server ws/frame.rs)
- Integration (PG-gated, `--ignored`): 5 storage + 3 im-core recall tests (all pass against live PG)
- Migration: fresh-DB replay of full chain incl. 0238 via `scripts/test-integration.sh`

## Commands Executed

```yaml
commands_executed:
  - command: "cargo check --workspace --all-targets"
    result: passed          # final run after all edits
  - command: "cargo clippy --workspace --all-targets -- -D warnings"
    result: passed
  - command: "cargo test --workspace --lib"
    result: passed          # 2158 passed, 0 failed (incl. new Recalled-arm materialization test)
  - command: "bash scripts/web-check.sh"
    result: passed          # 0 violations, 57 files (incl. new render_recall.test.js)
  - command: "bash scripts/truth-check.sh"
    result: passed          # 0 orphans (3 pre-existing allowlisted UNWIRED builders)
  - command: "bash scripts/file-size-check.sh"
    result: passed          # 0 violations
  - command: "bash scripts/test-integration.sh"
    result: passed          # fresh-DB migration regressions (incl. 0238) + full ignored suite, 594 passed 0 failed, EXIT 0
  - command: "cargo test -p aero-server --test authz_lint"
    result: passed          # 6 passed
  - command: "node --test web/*.test.js"
    result: passed          # 82 passed, 0 failed (incl. new ws/api/render recall tests)
  - command: "cargo test -p aero-storage --lib -- --ignored message::recall"   # throwaway migrated DB
    result: passed          # 9/9 (incl. concurrent_double_recall_has_exactly_one_winner + 3 gate regressions)
  - command: "cargo test -p aero-im-core --lib -- --ignored"                   # throwaway migrated DB
    result: passed          # 28 passed 0 failed (incl. 3 recall service tests)
  - command: "cargo run --bin aero-cli -- migrate" (throwaway DB)
    result: passed          # full 238-migration chain incl. 0238
  - command: "E2E live-server smoke (two WS clients, REST recall roundtrip)"
    result: not_executed    # see residual risks
```

## Architecture / Security Checks

```yaml
architecture_checks: passed   # module ownership per plan; no new crate/deps/tables/indexes; outbox/NATS/hub path reused
security_checks: passed       # authz_lint green; participant-first guards; commit-time FOR UPDATE role re-check; no cross-tenant oracle (404/403 before state 409)
compatibility:
  breaking_change: false      # additive columns (serde defaults), new enum variant covered by in-repo matches, new endpoint/frame are additive
migration:
  required: true
  rollback_verified: false    # DROP COLUMN + CHECK revert is trivial & additive; verified forward on fresh DBs only
```

## Residual Risks

- **E2E live-server smoke not executed** (not_executed): the full WS fan-out + REST roundtrip was not exercised against a running `aero-server` binary. Mitigated by: contract test asserting the exact wire frame, storage/service integration tests asserting the outbox row and placeholder payload, and the hermetic web render path. Remaining risk: hub/fan-out integration with a live NATS bus for this specific event (the bus path itself is unchanged code reused from Edited/Deleted).
- **`materialize_outbox_payload` Recalled arm** now has a dedicated unit test covering all four branches (delivered/superseded/tombstoned/gone) — the prior indirect-coverage risk is closed.
- **Rollback not executed** — migration is additive and reversible (drop 2 columns, revert CHECK), but no rollback run was performed.
- **`is_guest`/deactivated-participant recall edge**: recall permission re-check reads `room_members.role`; the effective-access fence (`aero_effective_room_access`) already rejects deactivated/guest-ineligible callers before the role read, same as delete.
- **Web recall affordance is author-only in the UI** (no room-role context client-side); admin/owner recall works via REST/WS with server-enforced authorization, matching how delete is surfaced.
- **`ws.test.js` crossed 800 lines** (831) — WARN-level advisory only, under the 1000-line HARD budget; web-check reports 0 violations.

## Assumptions

- "moderator" maps to the existing room roles `owner`/`admin` (the role enum is exactly `owner/admin/member`; there is no separate moderator role).
- Recall is deliberately **not** silently idempotent: a second recall returns stable 409 `conflict "message is already recalled"` per the requirement's failure-path list; clients treat it as already-effected.
- Recalled messages remain readable in history/search (placeholder); `searchable_text` is cleared so the placeholder never matches FTS.
- Pre-recall content is preserved as evidence via `message_edits` (editor = recaller) + `audit_events` — deliberately reusing existing evidence tables instead of a new `message_recalls` table (documented in the plan).
- Attachment bytes of recalled messages are garbage-collected via the existing `blob_gc_queue` (with live-reference check).
- `git` working tree left dirty per instructions (no commit); pre-existing `docs/DECISIONS.md`/`docs/campaigns/queue.out` modifications and untracked `docs/proposals/` were present before this task and untouched.
