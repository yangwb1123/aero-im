# Fix Plan — Recall F1 (index resurrection) + F3 (rate gate bypass)

Source: security review stage 02/06 (`docs/pi-batch/reviews/stage-02.out.md`, findings F1/F3).
Baseline gates run 2026-08-06 against current HEAD: all green (check / clippy `-D warnings` /
`cargo test --workspace --lib` 319 passed, 602 ignored / web-check 0 violations /
truth-check 0 ORPHAN + 3 pre-existing allowlisted UNWIRED builders). No code modified in this stage.

## 1. Root causes (with evidence)

### F1 — in-flight AI embed resurrects recalled content

Recall clears the index atomically (`authorization.rs:311-321`, the UPDATE fences
`recalled_at IS NULL AND deleted_at IS NULL`, sets `searchable_text = ''`, `embedding = NULL`),
but the **lockless worker index writes** that can land *after* that commit fence only
`deleted_at`:

| Site (verified) | Statement | Gap |
|---|---|---|
| `crates/aero-storage/src/message/crud.rs:459` (`update_embedding`) | `UPDATE messages SET embedding = $1 WHERE id = $2 AND deleted_at IS NULL` | post-recall write re-populates `embedding` → vector search (`search.rs:81-86`, `AND m.embedding IS NOT NULL`) returns original content |
| `crates/aero-storage/src/message/crud.rs:485` (`update_searchable_text`) | `UPDATE messages SET searchable_text = $1 WHERE id = $2 AND deleted_at IS NULL` | `search_tsv` is a STORED generated column → FTS (`search.rs:25-37`) re-derives original text |
| `crates/aero-storage/src/message/crud.rs:427` (`update_voice_transcript`) | appends transcript to `searchable_text`, `WHERE id = $1 AND deleted_at IS NULL` | same FTS resurrection (currently zero production callers — fence anyway, "EVERY index-update UPDATE") |

Race: the AI worker reads the message **without a lock** (`aero-ai/src/worker/mod.rs`,
`get` → `extract_attachment_text` → `update_searchable_text(id, folded)` at :293 →
`embed_text_with_context` → `update_embedding(id, embedding)` at :319), then writes it back
as two separate autocommit statements. A recall committing between read and write wins the
clear; the worker's later UPDATE then overwrites the cleared columns. Last-writer-wins.

The row-locked paths are already safe and are the model to copy: `events.rs:69-75`
(`edit_locked_outboxed_in_tx` Rust-checks `existing.recalled_at.is_some()` under
`lock_message_in_tx` FOR UPDATE) and `events.rs:194-200`
(`update_voice_transcript_outboxed`, same pattern — this is the path transcribe_bot
actually uses, `transcribe_bot.rs:161`). For lockless worker writes the **WHERE clause is
the fence** — exactly like `authorization.rs:319` (`AND recalled_at IS NULL`).

### F3 — recall bypasses rate gates

- WS: `ClientFrame::RecallMessage { id }` at `crates/aero-server/src/ws/ws_impl/frame.rs:157-159`
  calls `state.im.recall_message(pid, id)` bare. Compare `EditMessage` at
  `frame.rs:139-152`: `assert_message_edit_preflight` → `check_ws_rate_room` →
  `reserve_slowmode` → mutate.
- REST: `recall_message` at `crates/aero-server/src/routes/handlers/messages.rs:172-180` is
  bare; REST edit at `messages.rs:128-145` does the preflight → rate → slowmode sequence.
- `ImService::recall_message` (`crates/aero-im-core/src/service/messages.rs:460`) costs
  ~8-10 queries: `get` + `assert_room_access` + `role_of` + `recall_outboxed_authorized`
  (resolve → lock write access → lock message → role recheck under lock → recall UPDATE +
  history + audit + outbox + GC enqueue), incl. row locks. No per-workspace ceiling
  (`check_ws_rate_room`, `ws_rate.rs:345`, Redis fixed-window, fail-open) is charged.
- `check_ws_rate_room` doc (`ws_rate.rs:336-341`): **call only after `assert_room_access`**
  so non-members can't drain a victim workspace's budget → the recall gate must resolve the
  room through a preflight first, mirroring `assert_message_edit_preflight`
  (`im-core/service/messages.rs:368`). No `assert_message_recall_preflight` exists (grep = 0).

## 2. Module boundary & change radius (agent-guardrails.md §2)

- Direct files (5): `aero-storage/src/message/crud.rs` · `aero-storage/src/message/recall_tests.rs`
  · `aero-im-core/src/service/messages.rs` · `aero-server/src/ws/ws_impl/frame.rs` ·
  `aero-server/src/routes/handlers/messages.rs` (+ optional `aero-server/tests/authz_lint.rs`).
- Indirect impact: `aero-ai` worker writes become silent no-ops on recalled rows (no worker
  code change); search results can no longer surface recalled content (intended fix).
- Public interface: **no signature changes**. Storage methods keep signatures (return `false` /
  `None` for recalled rows — already the documented "missing/deleted" semantics). New
  `ImService::assert_message_recall_preflight(actor, id) -> Result<RoomId>` mirrors the edit
  preflight (new pub method, crate-internal consumers).
- DB / migration / config / events: **none**. Rollback = revert the 5 file edits.
- Deliberately NOT DRY-refactored (guardrails §3): the WS and REST recall arms duplicate
  edit's 4-line gate sequence, same as edit itself does today.

## 3. Exact changes

### F1 — `crates/aero-storage/src/message/crud.rs`
Add `AND recalled_at IS NULL` to the three lockless index UPDATEs, keeping `deleted_at`:
1. `update_embedding` (line ~459): `WHERE id = $2 AND deleted_at IS NULL AND recalled_at IS NULL`
2. `update_searchable_text` (line ~485): same
3. `update_voice_transcript` (line ~427): same (`WHERE id = $1 AND deleted_at IS NULL AND recalled_at IS NULL`)

Update the three doc comments to state the recalled fence (recalled rows are terminal;
post-recall worker writes are no-ops, matching `authorization.rs:319`).
Optional hardening (not required by the review, no behavior change): `AND recalled_at IS NULL`
on `list_without_embedding` (`message/query.rs:324-334`) — recalled rows are already excluded
via `searchable_text <> ''`, this makes it explicit.

### F1 — race test → `crates/aero-storage/src/message/recall_tests.rs`
Two new `#[ignore = "requires DATABASE_URL with migrations applied"]` tests using the existing
`Fixture`:
1. `recall_fences_late_index_writes` (deterministic): insert → recall → `update_embedding`
   ⇒ `false`, `update_searchable_text` ⇒ `false`, `update_voice_transcript` ⇒ `None`; row
   still `searchable_text == ''`, `embedding IS NULL`, placeholder blocks, version unchanged.
2. `concurrent_recall_vs_embed_write_never_resurrects` (race, mirrors
   `concurrent_double_recall_has_exactly_one_winner`): N≈8 iterations of
   `tokio::join!(recall_outboxed_authorized, update_embedding(+searchable_text))`; final-state
   invariant per iteration: `recalled_at IS NOT NULL ⇒ embedding IS NULL AND searchable_text = ''`
   (the WHERE fence makes this atomic; without it the worker write after the clear wins and the
   test fails).

### F3 — `crates/aero-im-core/src/service/messages.rs`
Add `assert_message_recall_preflight(actor, id) -> Result<RoomId>` mirroring
`assert_message_edit_preflight` (:368): `get` → `assert_room_access` → early `Conflict` for
deleted / already-recalled (same stable errors `recall_message` returns; the commit-time path
remains the authority). Doc comment: preflight is UX/room-resolution only, never authority.

### F3 — `crates/aero-server/src/ws/ws_impl/frame.rs` (:157)
`RecallMessage` arm becomes:
```rust
let room = state.im.assert_message_recall_preflight(pid, id).await?;
crate::ws_rate::check_ws_rate_room(state, room).await?;
state.im.recall_message(pid, id).await?;
```

### F3 — `crates/aero-server/src/routes/handlers/messages.rs` (:172)
Same two lines before `s.im.recall_message(...)` (edit's REST pattern at :128-145).

**Slowmode decision:** apply `check_ws_rate_room` only (the review's fix text names exactly
this for WS and "the REST rate limiter path used by edit_message" = the same call; REST edit
uses the identical `check_ws_rate_room`). Deliberately NOT `reserve_slowmode`: it gates
posting cadence off `messages.created_at` + a Redis reservation — applying it would block the
legitimate instant typo-recall in slow-mode rooms and adds a PG+Redis round-trip per attempt,
the opposite of the finding's cost concern. Flag as product decision; if strict edit parity is
wanted it is a 2-line addition (same `reserve_slowmode` call, result ignored or awaited).

## 4. Test plan

1. New storage race tests above — run: `DATABASE_URL=... cargo test -p aero-storage --lib -- --ignored message::recall`
   (fresh throwaway DB per AGENTS.md §4.3).
2. F3: new im-core DB-gated test for `assert_message_recall_preflight` (room resolution,
   member vs non-member, deleted/recalled early conflicts) in
   `crates/aero-im-core/src/db_tests/recall_tests.rs`.
3. Optional hermetic regression gate: extend `crates/aero-server/tests/authz_lint.rs` with a
   scan rule "handlers invoking `im.recall_message` must contain `check_ws_rate_room(`"
   (same source-scan style as the existing guard lint; keeps F3 from silently regressing).
   If out of scope, the review's explicit asks (F1 fence + race test, F3 gate) are still met.
4. Full gates (all must stay green; currently green):
   `cargo check --workspace --all-targets` ·
   `cargo clippy --workspace --all-targets -- -D warnings` ·
   `cargo test --workspace --lib` · `bash scripts/web-check.sh` · `bash scripts/truth-check.sh`.

## 5. Risk assessment

| Risk | Likelihood/Impact | Mitigation / gate |
|---|---|---|
| F1 fence changes worker write semantics for recalled rows (write silently no-ops) | Low; intended — recalled rows must never hold index content; worker already treats `updated=false` as benign ("row missing or deleted at update") | Race tests; existing 10 recall DB tests unchanged; backfill can't select recalled rows (`searchable_text=''`) |
| F1 fence missed on a future index path | Med | The 3 sites are the complete inventory (verified: every other `UPDATE messages` clears index columns — tombstones/erasures `participant.rs:545,636`, `sweep.rs:36`, `delivery_cursor.rs:406`, `message_edit.rs:285`, `bookmark.rs:269`); optional search-side `AND m.recalled_at IS NULL` as defense-in-depth |
| F3 preflight adds ~2-3 queries per recall attempt before the rate check | Low; identical cost profile to edit's preflight; recall was ~8-10 queries uncapped before | Per-workspace tier ceilings (defaults 1200-6000/min, Redis fail-open); preflight rejects unknown/non-member/terminal targets *before* charging, so they cannot drain budget |
| Legit recall bursts now rate-limited in hot workspaces | Low-Med; same exposure edit already has | Generous tier defaults; 429/error-frame is the documented, counted outcome (`WS_RATE_REJECTIONS_TOTAL`) |
| Slowmode parity question (product) | Behavior choice, not a defect | Documented decision above; one-liner to flip |
| New lint (authz_lint extension) red-flags false positives | Low if rule is narrow (recall handlers only) | Hermetic, no DB; CI gate catches regressions |

No lints disabled, no fences weakened, no migrations, no new dependencies. Rollback = revert 5 files.
