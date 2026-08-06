# Database Architect Review — Message Recall (0238)

**Deviation note:** `prompts/README.md` does not exist (no `prompts/` dir in the repo; searched repo root and workspace). The prompt body was self-contained, so I proceeded. All evidence below was re-derived from source and a fresh throwaway-DB replay (238/238 migrations apply cleanly; DB dropped after).

## 1. Store inventory (recall feature surface)

| Store | Purpose | Durable / Hot | Impl + stock wiring | Consistency req |
|---|---|---|---|---|
| PG `messages` | Recall target: placeholder blocks, `searchable_text=''`, `embedding=NULL`, `recalled_at/by`, `version+1` | Durable, hot | `recall_outboxed_authorized` (`aero-storage/src/message/authorization.rs`) in one tx with history/audit/outbox; wired via server pool | Row identity immutable; one-shot transition |
| PG `event_outbox` | Durable room-event queue, `kind='recalled'` | Durable (queue), hot | Relay worker spawned unconditionally in `bin/boot/background.rs` (250 ms, `AERO__SERVER__EVENT_OUTBOX_POLL_MS=0` opt-out); SKIP LOCKED lease 30 s | Per-message aggregate order; per-subject seq exactly once |
| NATS JetStream `im.room.{id}` | Cross-instance fact source; durable consumer `aero-server` | Durable | `run_bus_listener` (two-phase decode; poison ack-drop) | At-least-once, seq-stamped |
| Redis | Per-subject seq; AI answer-cache invalidation on `Recalled` at bus chokepoint (`bus.rs:457`) | Hot, fail-open (seq gap allowed) | Wired via fred + `AiContextStore` | Monotonicity best-effort |
| Hub (mpsc) | In-process WS fan-out | Ephemeral | Wired | — |
| PG `message_edits` / `audit_events` | Pre-recall body snapshot + `message.recalled` audit | Durable, cold-read | Same tx; no retention sweep (audit-evidence stance, `message_edit.rs:107`) | Evidence, not loss |
| PG blob GC | Recall enqueues now-unreferenced attachments | Durable queue | `blob_gc_drain` timer; live-ref check + cancel; delete-then-ack | — |
| `messages_partitioned` | Partition-cutover staging shadow | Cold, **not live** (cutover not in auto chain) | Mirror columns + reconcile in 0238; **backfill fn stale — finding 1** | Column-set parity with live |

**Stock binary wiring: verified yes** for every live path (relay, bus listeners, blob GC, side effects). No new store introduced by the feature.

## 2. Findings

### F1 — HIGH (latent, cutover-time data loss): backfill projection omits recall columns
`backfill_messages_partition` was last reissued in 0174 with an explicit column list; 0238 adds `recalled_at`/`recalled_by` to the shadow + reconciles **existing** shadow rows, but never reissues the function. The runbook's "Schema mirror invariant" (`docs/runbooks/messages-partitioning.md` §Step C) requires shadow + backfill projection + final sync + §5 verification in the same change — 0158's header documents the exact bug class this produces (silent fallback to column DEFAULT).

**Empirically demonstrated** (throwaway DB, transaction-wrapped 238-file replay, then recall → backfill):

```
 shadow_recalled | shadow_recalled_by | version |    body
-----------------+--------------------+---------+------------
 f               | f                  |       3 | [recalled]
```

Post-cutover consequences: "已撤回" badge disappears, `recalled()`/edit guard (`recalled_at IS NULL`) makes the placeholder **editable again**, `context.js` recall replay breaks, `recalled_by` audit linkage lost. Rows backfilled **after** 0238 runs are affected (0238's reconcile only fixes rows present at migration time).

**Fix:** amend 0238 (not yet deployed — branch `aero-r3-recall`) to reissue `backfill_messages_partition` with `recalled_at, recalled_by` added to both INSERT and SELECT lists (mirror 0158/0174), and extend runbook §5 verification. If 0238 ships as-is, add 0239 before any cutover.

### F2 — MEDIUM (rolling-deploy window): old binaries silently ack-drop `recalled` events
`run_bus_listener` is poison-safe: an undecodable payload is ACK-dropped (`BUS_POISON_DROPPED_TOTAL`), never nacked. A pre-recall node receiving a `recalled` event drops it — clients on that node never render the placeholder, and the durable cursor advances (no replay later). Only new binaries produce the event, but a mixed fleet (new producer + old consumer node) has silent-drop windows until the old node restarts. Acceptable design trade-off; document the deploy ordering (all binaries upgraded before recall use) rather than change the poison policy.

### F3 — LOW: `recalled_by` not in GDPR erasure list
`participant.rs` erasure anonymizes sender-keyed content + `message_edits` bodies but not `recalled_by` (or recaller `editor_id` in `message_edits` for others' messages). This converges with the deliberate "audit/governance records kept" stance (`message.recalled` audit row is also kept) — but the project invariant says every participant-keyed table must be *wired or explicitly reasoned*; add a comment, or `UPDATE messages SET recalled_by = NULL WHERE recalled_by = $1` if recaller identity is product-PII.

### F4 — LOW: reactions/block_interactions survive recall
`toggle_reaction` (`service/reactions.rs`) has no `recalled_at`/`deleted_at` guard; UI hides the react button for recalled messages (`render.js:787`), so only API-level. Slack clears reactions on un-send — decide and either document or clear in the recall tx.

### F5 — LOW: forward of a recalled message forwards the placeholder
`forward.rs` filters only `deleted_at` — forwarding copies the "此消息已被撤回" placeholder text. No leak (placeholder is public), but provenance looks odd; document or block.

### Observations (no action required)
- Recall of an expired-but-unswept message is allowed; retention sweep tombstones it later. Harmless.
- Recall is one-shot non-idempotent by design (409 on retry after ambiguous failure); REST retry can map 409→success client-side.
- `recalled_at` is the order key, `edited_at` not bumped — consistent with `context.js` replay.
- Attachments: `enqueue_unreferenced_blobs_in_tx` runs in the same tx; blob GC's live-reference check + cancel covers the attach/delete race.

## 3. Query / index & transaction analysis (hot & atomic paths)

**Recall tx** (`authorization.rs:206`): resolve target (no lock) → `aero_effective_room_access` fence (workspace→room→membership, migration 0197 fn) + `rooms FOR SHARE` → `FOR UPDATE` on the message row → identity revalidation (room/sender unchanged) → role re-check under `room_members … FOR UPDATE` (TOCTOU guard) → `UPDATE messages … WHERE id=$4 AND recalled_at IS NULL AND deleted_at IS NULL RETURNING …` (final fence; `None` ⇒ 409 "raced") → `message_edits` INSERT (pre-recall body, `editor_id`=recaller) → blob enqueue → `message.recalled` audit → outbox INSERT (`aggregate_version = MAX+1` — gap-free because the message row lock serializes all mutation kinds) → commit. Isolation: Read Committed + explicit locks in workspace→room→message order; no existence oracle (access guard precedes state checks).

**Relay atomicity** (`event_outbox.rs`): claim = `FOR UPDATE SKIP LOCKED` with `attempts` fencing + 30 s lease + per-message aggregate precedence (`NOT EXISTS earlier version unpublished`) + per-subject message-ordinal precedence; materialize = rebuild payload from current row, **suppress stale `Recalled`** when version superseded or row gone (mirrors Edited); seq = `COALESCE(seq,$2)` assign-once; publish idempotent via stable `event_id` as NATS Msg-Id; `mark_published` fenced on `attempts`; failure re-parks with backoff 1 s→300 s cap, no dead-letter (by design). `Recalled` rows carry `delivery_ordinal NULL` and skip the ordinal-precedence check but never overtake the create event (aggregate check).

**Index coverage** — all claim predicates covered: `idx_event_outbox_pending (available_at, created_at, id) WHERE published_at IS NULL`; `idx_event_outbox_aggregate_pending (message_id, aggregate_version) WHERE published_at IS NULL`; `idx_event_outbox_message_delivery_pending (subject, delivery_ordinal) WHERE event_kind='message' AND published_at IS NULL`; `idx_event_outbox_published (published_at)` for the sweep. Recall's `UPDATE` is PK-point. Partial GIN/HNSW indexes exclude `deleted_at` rows; recalled rows have `''` searchable_text (no FTS/trgm hits) and `NULL` embedding (excluded from vector ops) — recall removes content from both search paths. `search_tsv` is STORED-generated from `searchable_text` (0128), recomputed on the recall UPDATE.

## 4. Safe migration sequence

1. **Compatibility window:** 0238 is purely additive (2 nullable cols + CHECK drop/add extending 0211's set — verified all 7 kinds retained). Old binaries ignore new columns; only new binaries write `kind='recalled'`. Fresh-DB replay of the full 238-file chain verified (transaction-wrapped, mirroring sqlx).
2. **Validation queries** (post-deploy):
   ```sql
   SELECT count(*) FROM messages WHERE recalled_at IS NOT NULL;            -- 0 before feature use
   SELECT event_kind, count(*) FROM event_outbox GROUP BY 1;               -- 'recalled' appears
   SELECT count(*) FROM event_outbox WHERE published_at IS NULL AND available_at < now() - interval '10 min'; -- no stuck rows
   SELECT count(*) FROM messages_partitioned p JOIN messages m USING (id)
    WHERE m.recalled_at IS NOT NULL AND p.recalled_at IS NULL;             -- F1 parity check (must be 0)
   SELECT count(*) FROM message_edits e JOIN messages m ON m.id = e.message_id WHERE m.recalled_at IS NOT NULL AND e.blocks = m.blocks; -- snapshot sanity
   ```
3. **Roll-forward:** re-run 0238 — idempotent (`IF NOT EXISTS` + drop/add CHECK + NULL-safe reconcile).
4. **Rollback** (requires binary revert **first**, then):
   ```sql
   DELETE FROM event_outbox WHERE event_kind = 'recalled' AND published_at IS NULL;  -- else old relay decode-fails on the row and its aggregate blocks later events
   ALTER TABLE messages DROP COLUMN recalled_at, DROP COLUMN recalled_by;
   ALTER TABLE messages_partitioned DROP COLUMN recalled_at, DROP COLUMN recalled_by;
   -- restore 0211 CHECK (drop + add without 'recalled')
   ```
   Published `'recalled'` outbox rows may remain (CHECK only evaluated on writes); they're sweep-eligible.
5. **F1 gate:** apply the amended/0239 backfill function **before any partition cutover**; cutover is not in the auto chain (0148 hard-stop), so no urgent action — but the function fix must not be forgotten (0158 precedent).

## 5. Unknowns / assumptions

- **Volume:** no load evidence for recall throughput; `aero_messages_recalled_total` + `MESSAGE_PROCESSING_DURATION_SECONDS{op=recall}` exist — baseline these before and after go-live; outbox backlog gauge exists for relay pressure.
- **Recall window:** no time limit (open product decision, per plan) — a bounded window would change the permission matrix and add a `recalled_window` CHECK.
- **`message_edits` growth:** recall adds one row per recall, no retention sweep (evidence stance). Unbounded growth on high-recall tenants; no measurement of row size (blocks JSONB) × recall rate.
- **GDPR export:** exports the placeholder + `recalled_at`, not the pre-recall body (original lives in `message_edits`, excluded from export). Confirm this satisfies the product's erasure/export reading.
- **Partition cutover:** unscheduled; backfill scale unknown — runbook prescribes pre-creating historical monthly partitions; F1 must be resolved first.
- **F2 rolling-deploy behavior** (silent event drop on old nodes) — accepted, document in release notes.
