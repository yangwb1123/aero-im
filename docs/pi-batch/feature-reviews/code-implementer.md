## Handoff Report

### 1. Outcome and behavioral changes

The workspace already carried a recall implementation that the backend gate **rejected (FAIL)** with 5 blocking defects. I resolved all five, per the gate's prescribed fixes, and re-ran every gate:

1. **P1 — content resurrection via system edit**: `edit_locked_outboxed_in_tx` and `update_voice_transcript_outboxed` (`aero-storage/src/message/events.rs`) now fence `recalled_at` alongside `deleted_at`. A late unfurl link-preview or transcribe write returns `None` and can never rewrite the placeholder with the original body.
2. **P1/P2 — change-replay never delivers recalls**: `changes_since` keys on `GREATEST(edited_at, deleted_at, recalled_at)` (WHERE + ORDER BY); migration 0238 reissues `idx_messages_room_mutated` with that expression. Offline/reconnecting clients now converge on recalls.
3. **P2 — client dedupe drops the placeholder**: `handleIncomingMessage` routes held-id mutation rows (edit/recall/delete from reconnect backfill) through `applyChange` (the out-of-order/resurrect-guarded funnel); identical at-least-once redeliveries are still deduped.
4. **P2 — SPA receive-only**: added `ws.recallMessage(id)` (`recall_message` frame), `api.recallMessage(id)` (`POST /api/messages/:id/recall`), a `↶` recall button on self messages (`render.js`), and the `wireMsgActions` handler with **409→success mapping** (already-recalled is the desired end state; `msg:recalled` broadcast/replay converges the row).
5. **HIGH — partition-cutover backfill omits recall columns**: migration 0238 reissues `backfill_messages_partition` with `recalled_at`/`recalled_by` in the explicit projection (0148/0149/0158/0174 convention).

The recall migration chain (238 migrations) replays cleanly on a fresh DB; the shared dev DB was never touched (pre-existing checksum mismatch at 0172 blocks `aero-cli migrate` there — unrelated, left as-is).

### 2. Modified files (my changes; batch's recall work was already in tree)

| File | Purpose |
|---|---|
| `crates/aero-storage/src/message/events.rs` | `recalled_at` fences in both system-edit paths (P1) |
| `crates/aero-storage/src/message/query.rs` | `changes_since` includes `recalled_at` in mutation predicate/order (P2) |
| `migrations/0238_message_recall.sql` | Reissues `idx_messages_room_mutated` (3-col expression) + `backfill_messages_partition` with recall columns (P2, HIGH) |
| `crates/aero-storage/src/message/recall_tests.rs` | 3 new regression tests: system-edit fence, `changes_since` delivers recalls, backfill carries recall columns (scoped `from_id` cursor) |
| `web/app.js` | Held-id mutation convergence funnel; recall action with 409→success; compressed back to the 1000-line HARD limit |
| `web/ws.js` | `recallMessage(id)` → `{"type":"recall_message","id"}` |
| `web/api.js` | `recallMessage(id)` REST wrapper |
| `web/render.js` | `↶` recall button (self messages), distinct from reply `↩` |
| `docs/pi-batch/recall-completion-report.md` | Gate-fix round section documenting all five fixes + re-verification |

### 3. Tests and checks actually run

- `cargo check --workspace --all-targets` — **pass**
- `cargo clippy --workspace --all-targets -- -D warnings` — **pass**
- `cargo test --workspace --lib` — **2157 passed / 0 failed**
- `bash scripts/test-integration.sh` — **593 passed / 0 failed** on throwaway DBs (fresh 238-migration replay + full ignored suite serialized); recall storage suite 8/8, im-core 3/3, including the new regression probes
- `scripts/migrate_chain_smoke.sh` (fresh-DB chain replay incl. modified 0238) — **pass**
- `scripts/{web-check,truth-check,file-size-check}.sh` — **0 violations** (app.js back at exactly 1000 lines)
- web `node --test` (15 files) — **76 passed / 0 failed**
- Verified on migrated throwaway DB: index expression and backfill function carry recall columns

**Pre-existing, unrelated**: the shared dev DB `_sqlx_migrations` has a checksum mismatch at 0172 (blocks `aero-cli migrate` there); a parallel `--ignored` run without `--test-threads=1` shows concurrency/pollution failures (Snaplink runtime state) — both reproduce without my changes and the repo's own script avoids them.

### 4. Remaining limitations / risks / unverified assumptions

- **No time limit on recall window** and **no un-recall path** — deliberate product decisions left open (per plan); edit after recall stays blocked; moderation delete still applies to recalled rows.
- **Admin recall has no SPA button**: the web client has no room-role context, so the UI exposes recall for the author only; admin/owner recall works via WS/REST (server-enforced) — matching how delete is surfaced.
- **Real-browser E2E of the recall UI flow** not run (no browser harness); covered by render.js placeholder path, web-check import/syntax gate, and the `recalled_frame_shape_carries_placeholder_message` contract test.
- The backfill regression test is scoped via a `from_id` cursor (a whole-table sweep trips the shadow's blob-workspace-scope trigger on rows blob-fence tests deliberately leave cross-scoped); this is a test-design constraint, not a product defect.
- New `idx_messages_room_mutated` DROP+CREATE is safe (plain index, not a constraint) but momentarily drops the old index during migration on large tables.
