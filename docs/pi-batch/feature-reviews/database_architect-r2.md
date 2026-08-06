# Database Architect Review — Full-System Persistence (r2, post-recall)

**Prompt note:** `prompts/README.md` does not exist in the repo (no `prompts/`
dir; shared rules were read from `/home/u1/ai-batch-runner/prompts/README.md`).
Evidence = current tree at the message-recall completion revision (0238 in
chain). All claims below were re-derived from source at this revision; the
previous round-1 review (`database_architect.md`) covered the recall feature
surface and its F1 (stale backfill projection) is **verified fixed** in the
current 0238 — see F7.

Evidence labels: **Verified** = read in current source / ran; **Partial** =
inferred from adjacent code; **Unknown** = no evidence in tree.

---

## 1. Store inventory (hot vs durable, and stock wiring)

| # | Store | Purpose | Durable / Hot | Implementation + stock wiring | Consistency requirement |
|---|---|---|---|---|---|
| S1 | PG `messages` (+ children) | System of record: content, history, audit | **Durable**, hot read/write | sqlx `PgPool` (16 conns default, min 2, `statement_timeout=10s` per conn, `test_before_acquire`) — `aero-storage/src/db.rs`, booted in `bin/boot/persistence.rs` **Verified** | Row identity immutable; per-message mutation serialized by row lock |
| S2 | PG `event_outbox` | Transactional producer outbox (message/edit/delete/notify/reaction/canvas_op/**recalled**) | **Durable** queue (hot) | `EventOutboxRepo` claim/lease/seq/publish — `event_outbox.rs`; relay spawned unconditionally in `bin/boot/background.rs` (250 ms poll, `AERO__SERVER__EVENT_OUTBOX_POLL_MS=0` opt-out) **Verified** | Per-message aggregate order; per-subject seq exactly-once; at-least-once publish |
| S3 | PG `message_send_keys` / `message_side_effect` / `ai_jobs` / `webhook_delivery` / `blob_gc_queue` / `consumer_event_receipt` / `delivery_cursors` | Idempotency ledgers + worker queues + per-user delivery cursors | **Durable** queues/ledgers | `FOR UPDATE SKIP LOCKED` claim + attempts fencing (`ai_job.rs:180`, `webhook_delivery.rs:530` **Verified**); sweep timers in `bin/boot/retention.rs` **Verified** | Exactly-once side effects via (consumer,event_id) / business-unique keys |
| S4 | PG optional read replica | Capacity for explicitly eventual single-room reads only | Durable (replica) | `QueryRouter` (`query_router.rs` **Verified**) — only `QueryConsistency::Eventual` call sites; auth/security/cross-room/writes stay on primary; replica absent → natural fallback to primary **Verified** | Eventual; never authority |
| S5 | Redis (shared client) | Presence (room), stream viewer counts, call rosters, per-subject seq, ws-rate counters, short-lived cache | **Hot**; presence/roster/viewer = soft state (heartbeat TTL 30–45 s); seq = hard counter | `RedisCache` (fred) → `PresenceStore` 256-shard zsets, `StreamViewerStore` 256-shard, `CallRosterStore` single-key + Lua generation fencing, `SeqStore` `INCR` (`aero-storage/src/{presence,live_presence,seq,ws_rate,cache}.rs` **Verified**); all wired in `bin/boot/{repos,services}.rs` **Verified**; compose runs AOF (`--appendonly yes`) **Verified** | Presence: best-effort; seq: monotonic per subject, gaps legal, Redis-down → publish unstamped (fail-open, `outbox.rs`) |
| S6 | NATS JetStream | Cross-instance fact source | **Durable** streams (`im.room.*` Limits 7 d; `live.stream.*` 6 h; work-queue streams for bot/SFU lifecycle) | `JetStreamBus` (`aero-bus/src/jetstream.rs` — `max_age` 7 d/6 h, File storage, `Nats-Msg-Id` dedup window = max_age **Verified**); durable `aero-server` consumer per instance + ephemeral live consumer; wired in boot **Verified** | At-least-once; clients dedup/reorder by `seq` stamp |
| S7 | Blob stores | Attachment/HLS bytes | LocalFs default = **per-node disk (not cluster-durable)**; S3/AeroVault = durable object storage | `BlobStore` trait; `blob_store_from_env_checked` fail-loud on incomplete S3/vault config, unknown backend name errors, else `LocalFsBlobStore` (`s3_blob_store.rs:495-547` **Verified**); wired in `bin/boot/persistence.rs` **Verified**. Compose ships MinIO but the stock binary uses it **only** when `AERO_BLOB_BACKEND=s3` | Metadata tx + bytes delete-then-ack; live-reference check cancels ordinary deletes (`blob_gc.rs` **Verified**) |
| S8 | `messages_partitioned` | Partition-cutover staging shadow (NOT live; cutover deliberately not in auto chain) | Cold, staging | Created 0148, mirrors every live column (0238 adds recall cols + reconcile + reissues `backfill_messages_partition` **Verified**) | Column-set parity with live at cutover (runbook invariant) |
| S9 | Hub mpsc | In-process WS fan-out | Ephemeral | `hub.rs`, bounded channels; per-process | None (reconnect backfills from PG/NATS) |

**Stock-wiring verdict:** every durable path above is spawned by the stock
`aero-server` binary (outbox relay, bus listeners, retention sweeps, blob-GC
drain, integration-receipt sweep, gauges — all `bin/boot/`). No store exists in
the tree that the binary does not wire, with the deliberate exceptions: MinIO
(compose-only, env-gated), read replica (opt-in), and `messages_partitioned`
(staging, operator-invoked per runbook).

---

## 2. Findings

### F1 — HIGH (scale-conditional, release-blocker at volume): migrations run under the 10 s `statement_timeout`
- **Evidence (Verified):** `connect_pg` sets `SET statement_timeout = '10000'` on every connection via `after_connect` (`db.rs:33-51`); `migrate()` executes `sqlx::migrate!().run(pool)` on that same pool (`db.rs:65`); both `aero-server` boot (`boot/persistence.rs:30-33`) and `aero-cli migrate` (`aero-cli.rs:151-157`, pool size 4) use `connect_pg`. sqlx `Migrator` acquires connections from this pool, so every migration statement — including 0238's `CREATE INDEX idx_messages_room_mutated` (DROP+CREATE over all of `messages`) and its `UPDATE messages_partitioned … FROM messages` reconcile (full join of both tables) — is subject to the 10 s cap.
- **Impact:** on a production-sized `messages` table the 0238 index rebuild alone typically exceeds 10 s; the chain aborts mid-migration with `canceling statement due to statement timeout` and no resume until the statement is made fast enough. `CREATE INDEX CONCURRENTLY` is not an escape hatch because sqlx runs each migration inside a transaction. All gates are green only because the fresh-DB replays operate on empty tables. Severity is HIGH *if* volume is material (see §5 — no volume evidence in tree).
- **Recommendation:** run migrations on a dedicated connection/pool with `statement_timeout=0` (and `lock_timeout` left on), or document that the deploy window must pre-create the 0238 index outside the chain. Add a CI check that replays the chain against a synthetic `messages` table of ~1 M rows to bound migration wall-time.

### F2 — LOW/MEDIUM: `/changes` is single-page with no continuation; >200 mutations per reconnect window are lost forever
- **Evidence (Verified):** `changes_since` clamps `limit` to 200 (`message/query.rs:170-196`); the REST handler returns one page with no cursor (`routes/handlers/rooms.rs:room_changes`); `web/app.js:replayChanges` calls `roomChanges` once with `limit:200`, **advances the cursor to now before the reply**, and never loops.
- **Impact:** a client offline across >200 edits/deletes/**recalls** in one room never converges on the older mutations — including a recall of a message it still renders with original content. Bounded (per-room, per-reconnect window) and labeled best-effort, but the recall feature makes the stale-content consequence user-visible.
- **Recommendation:** loop with `since = last returned GREATEST(...)` while a page is full (the query is already ordered by mutation time, so this is a natural keyset continuation); or return `next_since` and make the web client page.

### F3 — LOW: no gauge for `event_outbox` pending depth
- **Evidence (Verified):** metrics samplers export DB pool, NATS consumer lag (`aero_nats_consumer_pending_messages`), AI DLQ (`bin/boot/metrics_tasks.rs`); no query samples `SELECT count(*) … WHERE published_at IS NULL` on `event_outbox` (grep of `metrics.rs`/`metrics_tasks.rs` for outbox: none).
- **Impact:** when NATS is down, failed publishes re-park with backoff (1 s→300 s, `event_outbox.rs`); the backlog grows invisibly until the 30 d sweep window — no alert path.
- **Recommendation:** add a gauge (cheap partial-index count, `idx_event_outbox_pending`) alongside the existing NATS-lag sampler.

### F4 — LOW: `recalled_by` survives GDPR erasure of the recaller
- **Evidence (Verified):** participant erasure anonymizes `messages.blocks/searchable_text/embedding` and `message_edits.blocks` (`participant.rs:545-585, 630-660`) but no statement NULLs `messages.recalled_by` (or recaller `editor_id` in `message_edits` for others' messages). `message.recalled` audit rows are deliberately retained (governance stance).
- **Impact:** a UUID of the erased recaller persists on message rows; consistent with the retained-audit stance but violates the "every participant-keyed table wired or explicitly reasoned" invariant without a comment.
- **Recommendation:** add `UPDATE messages SET recalled_by = NULL WHERE recalled_by = $1` in both erasure paths, or document the retention decision in `participant.rs`.

### F5 — INFO (documented boundary): NATS 7 d retention < outbox 30 d published retention; bots down >7 d lose events
- **Evidence (Verified):** `jetstream.rs:134` `max_age` 7 d for `im.room.*`; outbox published rows swept at `EVENT_OUTBOX_RETENTION_DAYS` (default 30, `retention.rs:88-101`); pending rows never swept. Relay never republishes `published_at IS NOT NULL` rows.
- **Impact:** a durable consumer (bot/ooo/unfurl/push/transcribe) down longer than the stream window permanently misses events; no replay path. Clients are unaffected (PG backfill + receipts). Acceptable at-least-once boundary; should be in the ops runbook.

### F6 — INFO: recalled rows stay in partial FTS/trgm GIN indexes
- **Evidence (Verified):** partial indexes are `WHERE deleted_at IS NULL` only (`0136_message_index_partial.sql`); recall sets `searchable_text=''` and `embedding=NULL` but `deleted_at` stays NULL, so the rows remain indexed (empty text → no matches; `search_tsv` regenerates empty). `messages_embedding_hnsw` excludes NULL embedding automatically.
- **Impact:** negligible index bloat proportional to recalled-message count; no correctness issue. Optional: extend the partial predicates with `AND recalled_at IS NULL`.

### F7 — RESOLVED (vs round 1): backfill projection now includes recall columns
- **Evidence (Verified):** current `0238_message_recall.sql` reissues `CREATE OR REPLACE FUNCTION backfill_messages_partition` with `recalled_at, recalled_by` in both the INSERT column list and SELECT list, and reconciles existing shadow rows (`UPDATE messages_partitioned … IS DISTINCT FROM …`). Round-1 F1 (silent fallback to column DEFAULT at cutover) is closed in this revision.

---

## 3. Query/index & transaction analysis (demonstrated hot/atomic paths)

### 3.1 Message insert (hot write) — `insert_outboxed` (`message/idempotency.rs:158-240`)
Single tx, **Verified**: `lock_effective_sender_room_access` (workspace→room→membership fence) → `lock_reply_parent_in_tx` (reply containment) → `BlobRepo::lock_message_attachments_in_tx` (attach/GC race fence) → row insert → outbox insert (`aggregate_version = COALESCE(MAX,0)+1`, gap-free because the message row lock serializes all mutation kinds) → `message_side_effect` rows (Notifications/Embed/Moderate) → idempotency-key claim (`ON CONFLICT DO NOTHING`, loser rolls back and returns the winner's pair). Commit = message + event + jobs + ledger atomic. Read Committed + explicit locks; no dual-write.

### 3.2 Recall (atomic state transition) — `recall_outboxed_authorized` (`message/authorization.rs:206-260`)
Resolve target (no lock) → effective-access fence (`rooms FOR SHARE`) → `FOR UPDATE` on message row → identity revalidation (room/sender immutable) → **role re-check under `room_members … FOR UPDATE`** (TOCTOU guard: concurrent demotion cannot slip through) → final fenced `UPDATE … WHERE recalled_at IS NULL AND deleted_at IS NULL RETURNING` (one-shot; `None` ⇒ 409) → `message_edits` snapshot (pre-recall body, `editor_id`=recaller) → attachment GC enqueue (same tx) → `message.recalled` audit → outbox `Recalled` append → commit. Post-commit fast dispatch (`dispatch_event_outbox_id`) is best-effort; the 250 ms batch relay owns durability.

### 3.3 Outbox relay (hot atomic consume) — `event_outbox.rs`
Claim = `FOR UPDATE SKIP LOCKED` + 30 s lease + `attempts` fencing; per-message aggregate precedence (`NOT EXISTS earlier.version unpublished`) and per-subject message-ordinal precedence; indexes cover every predicate (**Verified**): `idx_event_outbox_pending (available_at, created_at, id) WHERE published_at IS NULL` (claim order), `idx_event_outbox_aggregate_pending (message_id, aggregate_version) WHERE published_at IS NULL`, `idx_event_outbox_message_delivery_pending (subject, delivery_ordinal) WHERE event_kind='message' AND published_at IS NULL`, `idx_event_outbox_published (published_at)` (sweep). Seq = Redis `INCR` + `COALESCE(seq,$2)` assign-once; publish idempotent via stable `event_id` as `Nats-Msg-Id`; `mark_published` fenced on `attempts`; failure re-parks with exponential backoff 1 s→300 s cap (no dead-letter — by design); stale `Recalled`/`Edited` materialization suppressed when version superseded (`im-core/service/outbox.rs` **Verified**, incl. `recalled_payload_is_delivered_at_version_and_suppressed_when_superseded` test).

### 3.4 Reconnect convergence (hot read) — `changes_since`
`WHERE room_id=$1 AND GREATEST(edited_at, deleted_at, recalled_at) > $2 ORDER BY GREATEST(...) ASC LIMIT n` — matches the 0238-reissued expression index `idx_messages_room_mutated (room_id, GREATEST(edited_at, deleted_at, recalled_at))` exactly (**Verified**). Tombstones included by design; recall does not bump `edited_at`, so the reissued expression is the load-bearing fix that makes recalls visible to offline clients.

### 3.5 Cluster state (hot, soft) — Redis
Presence/viewer counts sharded 256 ways (`pid` low byte) to avoid hot-key serialization; read = prune `ZREMRANGEBYSCORE` + aggregate in parallel (**Verified**). Call roster single key with Lua generation fencing (`ROSTER_STAMP_GENERATION` compare_i64, hash-tag colocated side key for cluster mode) — late heartbeats/leaves from an older generation are rejected (**Verified**, incl. live-Redis ignored tests).

### 3.6 Side-effect queues
`ai_job`, `webhook_delivery` claim via `FOR UPDATE SKIP LOCKED` with attempts/backoff fencing (**Verified** `ai_job.rs:180`, `webhook_delivery.rs:530`); blob GC drains ≤50, live-reference check cancels ordinary deletes, GDPR `force_delete` bypasses, delete-then-ack only on Ok (**Verified** `blob_gc.rs`).

---

## 4. Safe migration sequence (current head = 0238)

### Compatibility window
- 0238 is purely additive for **new binaries**: 2 nullable columns (`recalled_at/by`), CHECK drop+add extending the 0211 kind set (all 7 kinds retained — verified in source), index reissue, backfill-function reissue, NULL-safe shadow reconcile. Old binaries ignore the new columns; **old binaries poison-drop `recalled` bus events** (unknown-variant ACK-drop, cursor advances) — deploy ordering must be: upgrade **all** nodes before recall use (documented in `docs/pi-batch/message-recall-plan.md`).
- The 0238 `UPDATE messages_partitioned … FROM messages` runs on the shadow only; empty shadow ⇒ instant. With a populated shadow this join + the `messages` index rebuild are the F1 wall-time risk.

### Validation queries (post-deploy)
```sql
-- recall state is written and visible
SELECT count(*) FROM messages WHERE recalled_at IS NOT NULL;
SELECT event_kind, count(*) FROM event_outbox GROUP BY 1;            -- 'recalled' present
-- relay health: no stuck pending rows (relay runs every 250 ms)
SELECT count(*) FROM event_outbox
 WHERE published_at IS NULL AND available_at < now() - interval '10 min';
-- shadow parity (F7 invariant): must be 0
SELECT count(*) FROM messages_partitioned p JOIN messages m USING (id, created_at)
 WHERE m.recalled_at IS NOT NULL AND p.recalled_at IS NULL;
-- snapshot sanity: pre-recall body preserved
SELECT count(*) FROM message_edits e JOIN messages m ON m.id = e.message_id
 WHERE m.recalled_at IS NOT NULL AND e.blocks = m.blocks;            -- 0 (blocks differ)
-- backfill projection carries recall columns (if shadow populated)
SELECT rows_copied FROM backfill_messages_partition(100, '00000000-0000-0000-0000-000000000000');
```

### Roll-forward
Re-run 0238 — idempotent (`IF NOT EXISTS`, drop/add CHECK, NULL-safe reconcile).

### Rollback (binary revert **first**, then)
```sql
DELETE FROM event_outbox WHERE event_kind = 'recalled' AND published_at IS NULL; -- else old relay decode-fails and blocks the aggregate
ALTER TABLE messages            DROP COLUMN recalled_at, DROP COLUMN recalled_by;
ALTER TABLE messages_partitioned DROP COLUMN recalled_at, DROP COLUMN recalled_by;
DROP INDEX IF EXISTS idx_messages_room_mutated;
CREATE INDEX idx_messages_room_mutated ON messages (room_id, GREATEST(edited_at, deleted_at));
```
Reissued `backfill_messages_partition` (now selecting recall columns) must also be replaced by the 0174 revision if the old binary could run it during the window. Data-integrity checks after rollback: `SELECT count(*) FROM event_outbox WHERE event_kind='recalled'` = 0; `\d messages` shows no recall columns.

---

## 5. Unknown volume, retention, recovery assumptions

- **Volume (Unknown):** no benchmarks, load tests, or `benches/` in the tree; no row-count or QPS evidence anywhere. Consequently: pool sizing (16), statement_timeout adequacy (F1), `/changes` page-size (F2), and shard-count adequacy (256) are all unvalidated at scale. **Measurements required:** peak messages/day per room, mutation rate per room (for F2), `messages` table size at deploy (for F1), outbox steady-state depth, Redis ops/sec on presence/seq keys.
- **Retention:** `messages` history is unbounded (soft-delete + audit stance; retention sweep only for ephemeral/expired); `event_outbox` published rows 30 d (raised to cover `MESSAGE_SEND_KEY_RETENTION_DAYS`), pending rows never swept; `message_edits`/`audit_events` never swept (evidence stance); NATS 7 d; Redis keys: presence/viewer/roster TTL-bounded, `aero:seq:*` **unbounded per subject** (deliberate — expiring would restart the sequence below delivered values; bounded by room/stream count).
- **Recovery (Partial/Unknown):** no backup/restore automation in `scripts/` or the Makefile; docker-compose persists PG/Redis/NATS/MinIO volumes under `./data/`; the only restore guidance is the runbook admonition ("tested restore … ultimate rollback", `docs/runbooks/messages-partitioning.md`). Recovery of message history depends on a PG backup strategy that is not in the tree. Redis loss ⇒ seq restart risk (mitigated by outbox `seq` persisted per row + client dedup) and presence/roster reset (self-healing via heartbeats); NATS loss beyond 7 d ⇒ bot-event loss (F5).
- **Production-readiness seams (Partial):** S3/vault blob backends are wired and fail-loud but never exercised against real endpoints in this environment; the read-replica path is opt-in and untested against an actual replica.

## Bottom line

Persistence architecture is sound and consistently implemented: transactional outbox with fenced, lease-based relay; SKIP LOCKED worker queues; cluster state in sharded Redis with generation fencing; fail-open/loud seams chosen deliberately; migration discipline (additive shadow, runbook cutover) is exemplary. The actionable items before any large-scale rollout: **F1 (migration statement timeout), F2 (/changes continuation), F3 (outbox depth gauge)**; F4 is a one-line erasure wiring decision; F5/F6 are documentation. No Critical findings. All round-1 recall findings re-verified: F1 fixed, F2/F3/F4/F5 carry over with the same mitigations.
