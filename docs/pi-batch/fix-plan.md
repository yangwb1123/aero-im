Plan persisted to `docs/pi-batch/fix-plan-recall-f1-f3.md`. Here it is in full:

---

# Fix Plan — Recall F1 (index resurrection) + F3 (rate gate bypass)

## 0. Reproduction (all gates run, baseline green)

| Gate | Result |
|---|---|
| `cargo check --workspace --all-targets` | ✅ clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✅ clean |
| `cargo test --workspace --lib` | ✅ 319 passed / 0 failed / 602 ignored (DB/Redis-gated) |
| `bash scripts/web-check.sh` | ✅ 0 violations |
| `bash scripts/truth-check.sh` | ✅ 0 ORPHAN; 3 UNWIRED = pre-existing allowlisted seams (`with_hls_dir`/`with_cost_model`/`with_rtmp_listen`, AGENTS.md §4.4) |

No code modified in this stage.

## 1. Root causes (file/line evidence)

**F1** — Recall clears the index atomically (`message/authorization.rs:311-321`, `WHERE recalled_at IS NULL AND deleted_at IS NULL`), but three **lockless worker index UPDATEs** fence only `deleted_at`:
- `crud.rs:459` `update_embedding` → `WHERE id = $2 AND deleted_at IS NULL` — post-recall write re-populates `embedding`; vector search (`search.rs:81-86`, `AND m.embedding IS NOT NULL`) resurfaces original content.
- `crud.rs:485` `update_searchable_text` → same WHERE — `search_tsv` is a STORED generated column, so FTS (`search.rs:25-37`) re-derives original text.
- `crud.rs:427` `update_voice_transcript` → appends transcript to `searchable_text`, same WHERE (zero production callers today; fence anyway).

Race: the AI worker reads without a lock and writes later as separate statements — `aero-ai/src/worker/mod.rs:293` (`update_searchable_text`) and `:319` (`update_embedding`) after `get`/`extract_attachment_text`/`embed_text_with_context`. A recall committing in between wins the clear; the worker's later UPDATE overwrites it (last-writer-wins). The row-locked paths (`events.rs:69-75` edit, `:194-200` `update_voice_transcript_outboxed` — the one transcribe_bot actually uses) are safe because they Rust-check `recalled_at` under `FOR UPDATE`; for lockless writes **the WHERE clause is the fence**, exactly like `authorization.rs:319`. I verified every other `UPDATE messages` in the workspace is a tombstone/erasure that clears index columns (`participant.rs:545,636`, `sweep.rs:36`, `delivery_cursor.rs:406`, `message_edit.rs:285`, `bookmark.rs:269`) — not F1 targets.

**F3** — WS `RecallMessage` arm (`ws/ws_impl/frame.rs:157-159`) and REST handler (`routes/handlers/messages.rs:172-180`) call `im.recall_message` bare; edit's arms (`frame.rs:139-152`, `messages.rs:128-145`) do `assert_message_edit_preflight` → `check_ws_rate_room` → `reserve_slowmode`. `recall_message` (`im-core/service/messages.rs:460`) costs ~8-10 queries incl. row locks with no per-workspace ceiling. `check_ws_rate_room` (`ws_rate.rs:345`) documents "call only after `assert_room_access`" so non-members can't drain a victim's budget — hence a recall preflight is needed (none exists; grep = 0).

## 2. Module boundary / change radius (agent-guardrails.md §2)

- **Direct (5 files)**: `aero-storage/src/message/crud.rs` · `aero-storage/src/message/recall_tests.rs` · `aero-im-core/src/service/messages.rs` · `aero-server/src/ws/ws_impl/frame.rs` · `aero-server/src/routes/handlers/messages.rs` (+ optional `aero-server/tests/authz_lint.rs`).
- **Indirect**: aero-ai worker writes silently no-op on recalled rows (no worker change); search can no longer surface recalled content (intended).
- **No signature changes, no migration, no config/event changes, no new deps.** Rollback = revert 5 files. No DRY refactor of the gate sequence (edit already duplicates it per arm; guardrails §3).

## 3. Exact changes

1. **F1**: `AND recalled_at IS NULL` added to the 3 lockless UPDATEs in `crud.rs` (keep `deleted_at`); update their doc comments. Optional hardening: same predicate on `list_without_embedding` (`query.rs:324`, already excluded via `searchable_text <> ''`).
2. **F1 tests** (`recall_tests.rs`, DB-gated like existing 10): `recall_fences_late_index_writes` — recall first, then all three writes must return `false`/`None` with row/version untouched; `concurrent_recall_vs_embed_write_never_resurrects` — N≈8 × `tokio::join!(recall, update_embedding, update_searchable_text)` with invariant `recalled_at IS NOT NULL ⇒ embedding IS NULL AND searchable_text = ''` (atomic thanks to the WHERE fence; fails without it).
3. **F3**: new `ImService::assert_message_recall_preflight(actor, id) -> Result<RoomId>` (get → `assert_room_access` → early deleted/recalled Conflicts; commit path remains authority). WS arm + REST handler each prepend `assert_message_recall_preflight` → `check_ws_rate_room` → `recall_message`.
4. **Slowmode decision**: `check_ws_rate_room` only — the review's fix text names exactly that for WS and the REST edit path uses the identical call. `reserve_slowmode` deliberately **not** applied: it gates posting cadence off `messages.created_at` + Redis reservation, which would block legitimate instant typo-recalls in slow-mode rooms and add PG+Redis cost per attempt. Flagged as a product decision; 2 lines to flip.
5. **F3 tests**: DB-gated im-core test for the preflight; optional hermetic `authz_lint.rs` scan rule ("recall handlers must contain `check_ws_rate_room(`") to keep the gate from regressing.

## 4. Risk assessment

- **F1 write no-ops on recalled rows**: intended; worker already treats `updated=false` as benign; backfill can't reselect recalled rows; existing 10 recall DB tests unchanged → race tests + recall suite protect.
- **Missed index path**: inventory is complete (verified exhaustively); optional search-side `AND m.recalled_at IS NULL` as defense-in-depth.
- **F3 preflight adds ~2-3 queries**: same profile as edit's preflight; unknown/non-member/terminal targets are rejected *before* charging so they can't drain budget; tier ceilings are generous (1200-6000/min, Redis fail-open); 429/error-frame is the documented counted outcome.
- **Legit recall bursts now capped**: same exposure edit already has; ceilings are tier-configurable.
- **No lints disabled, no fences weakened**; all five gates must stay green post-fix, plus the new DB-gated tests on a throwaway DB (`DATABASE_URL=... cargo test -p aero-storage --lib -- --ignored message::recall`).

The review's concrete asks (F1 `recalled_at IS NULL` on every index-update UPDATE + race test; F3 same gate as edit on both entry points) are met with zero migration and a 5-file radius.
