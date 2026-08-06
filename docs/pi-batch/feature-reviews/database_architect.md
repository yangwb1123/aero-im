# Database Architect Review — Aero IM

**Scope**: reviewed the recall feature (round-3 B1/B2, all gates green) plus the persistence layer it touches: `messages`/`message_edits`/`event_outbox`/`blob_gc_queue`/`blobs`, the outbox relay, GC drain, NATS bus, Redis stores, and stock binary wiring. Evidence is **Verified** from source at commit `fbff3d5` unless marked otherwise; checks ran: `cargo check/clippy/test --lib` (2159), storage recall suite 10/10 on fresh DB, `test-integration.sh` (238-migration replay), `node --test web/*.test.js` 82/0.

---

## 1. Store inventory

| Store | Purpose | Durability | Implementation | Stock wiring | Consistency requirement |
|---|---|---|---|---|---|
| **PostgreSQL 17** (`pgvector/pgvector:pg17`) | Source of truth: messages, rooms, membership, `event_outbox`, `message_edits`, `blobs` metadata, `blob_gc_queue`, audit, all repos | **Durable** (WAL; no backup/PITR config found — see §5) | `sqlx` pool: `max_connections` (default 24), `min 2`, `statement_timeout='10s'` on every connection, `test_before_acquire`; optional async read replica via `QueryRouter` (explicit eventual call sites only) | **Wired**: `boot/persistence.rs::connect` — `connect_with_retry` + `migrate()` (embedded, checksummed, transactional) at boot | Strong; per-aggregate serialization via row locks; fences in `aero_effective_room_access` (0197) |
| **Redis 7** (`redis:7-alpine`, `--appendonly yes` in compose) | **Hot** cluster state: presence, `StreamViewerStore` (sharded zsets + heartbeat eviction), `CallRosterStore`, per-subject `seq` counters (`aero:seq:{subject}`, **deliberately no TTL**), rate limits, participant cache | Semi-durable: AOF in compose; keys are TTL/heartbeat-rebuilt by design (loss-tolerant) except `seq` counters, which **must not reset** | `fred` 9 | **Wired**: `boot/persistence.rs` (required at boot, `connect_with_retry`) | Eventual for presence/rosters; per-subject monotonic seq; Redis outage → rate-limit fail-open |
| **NATS JetStream 2.10** | Durable event facts + hot fan-out | **Durable** streams (File storage): `IM_MESSAGES` `im.room.*` 7d retention + **7d duplicate window**; `IM_EVENTS` 30d; `AI_QUEUE` WorkQueue 24h; `LIVE_EVENTS` 6h/200k | `async-nats`; per-instance durable `aero-server-*` consumers (`DeliverPolicy::New`), queue-group durables for bots (`DeliverPolicy::All`), ephemeral for live; poison DLQ = `max_deliver 16` / `ack_wait 120s` | **Wired**: `run_bus_listener` (im.room.*, per-instance durable), `run_live_bus_listener` (ephemeral), 7 bots + webhook dispatcher | At-least-once; per-subject seq stamps; broker-level dedup window is *not* the only idempotency boundary (durable PG receipts are) |
| **Blob bytes** | Attachment/Voice/emoji objects | **Durable**; default `LocalFsBlobStore`; `AERO_BLOB_BACKEND=s3` (real SigV4 reqwest) / `vault` — fail-loud on incomplete config, never silent fallback | `BlobStore` trait; metadata in PG `blobs`; `region_blob_store` scope | **Wired**: `boot/persistence.rs` → `blob_store_from_env_checked` | delete-then-ack GC; `force_delete` (GDPR) vs reference-aware (`force_delete=false`) |
| **In-process** | Hub bounded `mpsc` fan-out, rate-limit buckets, `SfuRouter` | **Volatile** | tokio | boot | Best-effort realtime; per-key maps bounded by idle sweeps |

**Stock-binary wiring verdict**: all durable/hot stores are wired unconditionally at boot (`PG`, `Redis`, `NATS`, blob) with retry + graceful drain via shared `CancellationToken`; the blob-GC drain, outbox relay, live relay, webhook dispatcher, and sweeps are all spawned in `boot/background.rs`/`retention.rs`. The optional seams (`replica_url`, `AERO_BLOB_BACKEND=s3|vault`, `AERO_SAML_EXPERIMENTAL_VERIFY`) are env-gated but fail-loud or degrade deliberately — no dead wiring found.

---

## 2. Findings (severity-sorted)

### F1 — **Medium** — `changes_since` timestamp pagination has no unique tie-break; bulk same-timestamp mutations are silently skipped on reconnect
- **Path**: `crates/aero-storage/src/message/query.rs::changes_since` — `WHERE room_id=$1 AND GREATEST(edited_at, deleted_at, recalled_at) > $2 ORDER BY GREATEST(...) ASC LIMIT $3` (clamp 1..200); route `rooms.rs:335`; client `web/app.js:341` fetches **one page of 200** and advances the cursor (`state.lastChangeSync`).
- **Evidence (Verified)**: set-based sweeps stamp `deleted_at = NOW()` per statement (`crud.rs:263`, `events.rs:320`), so a retention/ephemeral sweep of >200 messages in one room yields >200 rows with an identical `GREATEST(...)`. Page 1 returns 200; the client's next `since` = that same timestamp; `> since` excludes **all** remaining rows → the last N mutations never converge on offline/reconnecting clients. Recall (0238) added a third timestamp column to this predicate but not a tie-break.
- **Impact**: bounded correctness gap in the reconnect mutation-replay path (live fan-out unaffected; history reads unaffected). Not data loss — but it is exactly the "offline clients showing pre-recall content forever" class of bug this feature was built to close.
- **Recommendation**: keyset tie-break `(GREATEST(...), id) > ($2, $3)` + composite index `(room_id, GREATEST(edited_at, deleted_at, recalled_at), id)`; or stagger sweep timestamps per row. **Validation**: PG regression with 250 rows sharing one `deleted_at`; assert all converge across pages.

### F2 — **Medium** — `message_edits` has no FK and no partition-cutover or hard-delete story; orphans accumulate
- **Evidence (Verified)**: `0036_message_edits.sql` — `message_id uuid NOT NULL` with **no `REFERENCES messages`**; ephemeral sweep hard-deletes rows (`sweep.rs:39` `DELETE FROM messages WHERE id = ANY($1)`) with no `message_edits` cleanup (only test code deletes; `participant.rs` only *anonymizes*). Migrations 0148/0174/0238 mirror live `messages` columns into `messages_partitioned` but never mention `message_edits`/`message_reports`, so the documented cutover (FK repoint + table swap) has no defined fate for edit/recall history.
- **Impact**: each recalled/edited ephemeral message leaves an orphan evidence row forever (small but unbounded); at cutover, history either silently orphans or is dropped depending on the runbook's unstated choice. Recall increases per-message `message_edits` rows.
- **Recommendation**: add `REFERENCES messages(id) ON DELETE CASCADE` (safe now that recall snapshots are redacted — the B1 invariant makes cascade deletion of redacted text correct) or an explicit sweep; add `message_edits` + `message_reports` to the cutover runbook checklist. **Validation**: `SELECT count(*) FROM message_edits e LEFT JOIN messages m ON m.id=e.message_id WHERE m.id IS NULL;`

### F3 — **Medium (operational)** — `statement_timeout='10s'` applies to the migration pool; heavy index statements will fail deploys at volume
- **Evidence (Verified)**: `db.rs` `connect_pg` sets `SET statement_timeout='10000'` via `after_connect` on **every** connection, and `migrate()` runs the embedded chain on that same pool. 0238's `DROP INDEX` + `CREATE INDEX idx_messages_room_mutated` (plain, non-CONCURRENT, inside the migration transaction) will exceed 10s on a large `messages` table → migration aborts mid-deploy.
- **Impact**: release blocker at scale; today's volumes are unmeasured (see §5), so currently latent.
- **Recommendation**: open heavy migrations with `SET LOCAL statement_timeout = 0` (transaction-scoped, safe under sqlx's per-file tx); document an env knob (`AERO_MIGRATION_STATEMENT_TIMEOUT`). Also note: `CREATE INDEX CONCURRENTLY` cannot run inside the transactional migration — plan a `-- no-transaction` split migration for the index reissue when volume demands.

### F4 — **Low** — Redis `seq` counter reset would violate per-subject monotonicity
- **Evidence (Verified)**: `seq.rs` — counters have no TTL (correct); compose runs Redis with AOF. But production Redis persistence is unverified; a restore from RDB-only snapshot restarts counters below already-delivered values → clients dedupe newer events as stale (same `(subject, seq)` seen twice).
- **Impact**: dropped fan-out under a Redis recovery; bounded by the 7d NATS duplicate window masking, but not by design.
- **Recommendation**: document a recovery check — after Redis restore, assert `aero:seq:{subject}` ≥ `max(seq)` in `event_outbox` per subject (or rely on AOF and say so in the runbook).

### F5 — **Info** — rolling-deploy behavior for `recalled` events is ack-drop on old binaries (already documented)
- **Evidence (Verified)**: `bus.rs` two-phase decode — unknown `kind` variant fails typed `RoomEvent` and legacy envelope, then **ACK-drops** as poison (`BUS_POISON_DROPPED_TOTAL`) rather than nacking forever. Old durable *bot* consumers nack → per-consumer poison at 16 deliveries.
- **Impact**: during a rolling deploy, old nodes drop recall events (clients on old nodes don't see the placeholder until reconnect replay). Bounded, documented in the completion report; deploy ordering discipline (new binaries first) is the mitigation. No code change recommended.

### F6 — **Info** — recall state machine is code-enforced, not DB-enforced
- **Evidence (Verified)**: 0238 comment states `recalled_at`/`deleted_at` coexistence is code-governed; the one-shot transition is enforced by `FOR UPDATE` row lock + `WHERE recalled_at IS NULL AND deleted_at IS NULL`. All production writers (send/edit/recall/delete/sweeps/system bots) go through `MessageRepo` methods; direct SQL could bypass.
- **Verdict**: acceptable and consistent with the delete path; note in schema docs that a future `CHECK (recalled_at IS NULL OR ...)` would need to accommodate the legit recall-then-delete sequence.

### Verified-correct (checked, no finding)
- **GC enqueue/drain race is closed**: the insert path (`blob.rs::lock_message_attachments_for_installation_in_tx`) takes `FOR SHARE` on referenced blobs (sorted id order, deadlock-safe vs. the enqueue's `FOR UPDATE`) **and rejects any blob present in `blob_gc_queue`** (`NOT EXISTS` + length check → Forbidden). Enqueue (recall/delete) holds the message row lock + blob `FOR UPDATE`; once a blob is queued, no new live reference can be created through any repo path (dedup also excludes queued blobs). The drain's `has_live_references` re-check is belt-and-braces, and `delete-then-ack` preserves the "no broken dedup hits" invariant.
- **B1 redaction**: `redact_blocks_for_recall_snapshot` (File→`[附件已移除]`, Voice+transcript→transcript text, Voice w/o→`[语音已移除]`); regression `recall_snapshot_redacts_blob_references_and_gc_proceeds` asserts zero `"blob_id"` occurrences in the snapshot, both blobs in `blob_gc_queue`, placeholder in the live row, transcript preserved. `has_live_references` scans `messages`/`custom_emoji`/`workspace_emoji`/`integration_blob_ledger` — **not** `message_edits` — so the invariant is load-bearing and correct.
- **Recall fencing**: user edit, system edit (unfurl/transcribe), and voice-transcript paths all check `recalled_at` under the row lock (`events.rs:84-87,192`); recall bumps `version` so stale optimistic edits Conflict; `changes_since` regression test covers offline replay.

---

## 3. Hot / atomic path analysis

**Message send (single tx, `idempotency.rs::insert_outboxed`)** — fence `aero_effective_room_access` (0197, workspace→room `FOR SHARE`→membership `FOR UPDATE` order) → reply-parent `FOR KEY SHARE` (composes with 0190 composite FK) → attachments `FOR SHARE` (sorted, queue-excluding) → `INSERT messages` → `INSERT event_outbox` (`aggregate_version = MAX+1`, safe under the message row lock) → side-effect jobs → idempotency ledger `ON CONFLICT DO NOTHING` (loser rolls back and returns the winner's canonical pair). Indexes: `messages_room_created_idx` (hot `list_recent`), `messages_blocks_gin` (mentions/blob containment), `idx_event_outbox_pending` partial, `(message_id, aggregate_version)` unique.

**Outbox relay (atomic consume/rotate, `event_outbox.rs`)** — `claim_due`: CTE with `FOR UPDATE SKIP LOCKED`, per-message `NOT EXISTS earlier-unpublished` ordering + delivery-ordinal ordering for `message` events; 30s lease; exponential backoff 1s→5min; seq minted **once** via Redis `INCR` and persisted (`assign_seq_if_absent`) so retries reuse it; NATS publish with message-id dedup (7d window); `mark_published`; published rows swept after `AERO__SERVER__EVENT_OUTBOX_RETENTION_DAYS` (`retention.rs:466`). Verified: recall events ride this exact queue with aggregate ordering (message→edited→recalled→deleted can never overtake).

**Recall (single tx, `authorization.rs::recall_outboxed_authorized`)** — resolve target (no lock) → effective-access fence → `lock_message_in_tx FOR UPDATE` → identity re-validation → role re-check under membership lock (TOCTOU guard) → redacted snapshot into `message_edits` → `UPDATE messages` (placeholder, `searchable_text=''`, `embedding=NULL`, `version+1`, guarded `WHERE recalled_at IS NULL AND deleted_at IS NULL`) → GC enqueue (per-blob `FOR UPDATE` + containment check + `ON CONFLICT DO NOTHING`) → audit row → outbox append. One-shot: concurrent double-recall has exactly one winner (regression test).

**Live viewer/presence** — Redis sharded zsets (`live:viewers:stream:{id}:shard:{uid%256}`) with `zadd GT` + `zremrangebyscore` heartbeat eviction; per-subject seq via Redis `INCR` (gaps legal, monotonic required); ephemeral NATS consumer per instance (`DeliverPolicy::New`) — restart drops only unacked live events, never the PG history.

**Index inventory on the recall-relevant hot paths** — `idx_messages_room_mutated` expression index (reissued 0238 with `recalled_at`); `messages_blocks_gin` serves both the enqueue containment check and `has_live_references`; HNSW `messages_embedding_hnsw` for vector search; `message_edits_msg_idx (message_id, recorded_at DESC)` for history reads.

---

## 4. Safe migration sequence (for 0238 and successors)

**Current state**: 238 migrations, embedded at compile time (migrate → **build → migrate** order per AGENTS §4.2), each file transactional (no `-- no-transaction` markers), checksummed in `_sqlx_migrations`, run on primary at boot; replica receives schema via replication.

**Compatibility window** (Verified from 0238 content):
- Additive: two **nullable** columns (`recalled_at`, `recalled_by`) — old binaries ignore them, new binaries tolerate NULLs; `recalled_at IS NULL` = not recalled everywhere.
- `event_outbox_kind_check` DROP+ADD: brief `ACCESS EXCLUSIVE` on `event_outbox` (small, swept table — relay stall of milliseconds; acceptable).
- `idx_messages_room_mutated` DROP+CREATE: (a) brief window with no index (seq scan on `changes_since` — perf only), (b) **write-blocking index build** on `messages` — see F3; schedule for low traffic or split to CONCURRENTLY.
- `backfill_messages_partition` reissued via `CREATE OR REPLACE` (projection fix, no data migration); `messages_partitioned` reconcile UPDATE is shadow-driven (bounded by shadow size, idempotent, NULL-safe).

**Validation queries** (run post-deploy, pre-cutover):
```sql
-- 1. recall columns live + shadow reconciled
SELECT count(*) FROM messages WHERE recalled_at IS NOT NULL;
SELECT count(*) FROM messages_partitioned p
  JOIN messages m ON m.id = p.id AND m.created_at = p.created_at
 WHERE p.recalled_at IS DISTINCT FROM m.recalled_at;          -- expect 0
-- 2. outbox kind accepted + relay flowing
SELECT event_kind, count(*) FROM event_outbox GROUP BY 1;     -- 'recalled' present
SELECT count(*) FROM event_outbox WHERE published_at IS NULL; -- drains to 0
-- 3. index in use
EXPLAIN (ANALYZE) SELECT id FROM messages
 WHERE room_id = $1 AND GREATEST(edited_at, deleted_at, recalled_at) > now() - interval '1 day'
 ORDER BY GREATEST(edited_at, deleted_at, recalled_at) LIMIT 200;  -- expect Index Scan
-- 4. GC hygiene
SELECT count(*) FROM blob_gc_queue q
  JOIN messages m ON m.blocks @> jsonb_build_array(jsonb_build_object('blob_id', q.blob_id::text))
 WHERE NOT q.force_delete AND m.deleted_at IS NULL AND (m.expires_at IS NULL OR m.expires_at > now());
-- 5. data integrity (B1 invariant)
SELECT count(*) FROM message_edits e JOIN messages m ON m.id = e.message_id
 WHERE m.recalled_at IS NOT NULL AND e.blocks::text LIKE '%blob_id%';  -- expect 0
```

**Rollback**: 0238 is purely additive; rollback = deploy the previous binary (columns unused, `recalled` events then ack-dropped by old consumers — F5, so roll back *all* nodes together or accept the replay-convergence delay). Do **not** `DROP COLUMN` mid-flight (ACCESS EXCLUSIVE + partition-shadow re-mirror churn); defer any column drop to a maintenance window. **Roll-forward**: re-running is a no-op (`IF NOT EXISTS`, `ON CONFLICT DO NOTHING`, idempotent reconcile); the `event_outbox_kind_check` DROP+ADD is re-entrant by design (0211 convention).

**Data-integrity checks**: (4) and (5) above, plus the storage recall suite (10/10, incl. partition-backfill-carries-recall-columns) and the fresh-DB 238-migration replay already in `test-integration.sh`.

---

## 5. Unknown volume, retention, and recovery assumptions

- **Volume (all Missing)**: no benchmarks, load tests, or SLOs anywhere in the repo. Unknown: messages/day, active rooms, WS connections/instance, blob count/size distribution, NATS msg/s, Redis ops/s. The observability stack (`/metrics`, NATS-backlog and AI-DLQ gauges) is the measurement surface, but no thresholds are defined. **Required measurements before scale claims**: `messages` row count + index sizes, `event_outbox` pending depth (gauge exists), `blob_gc_queue` depth + drain throughput, `changes_since` p95 under a sweep storm, migration wall-time at production row counts (F3 trigger).
- **Retention**: message retention per channel/workspace (0009 + COALESCE defaults, sweep-gated by env); ephemeral hard-delete; `blob_gc_queue` drained ≤50/60s (backlog risk under mass GDPR/retention runs — unmeasured); `event_outbox` published rows swept at `EVENT_OUTBOX_RETENTION_DAYS` (default ≥ send-key retention); NATS `IM_MESSAGES` 7d/7d dup window, `LIVE_EVENTS` 6h/200k; `message_edits`/`message_reports` **unbounded** (F2).
- **Recovery (Missing)**: no pg_dump/PITR/WAL-archiving config or restore drill found in repo or docs; `data/pg` is a docker volume; the optional read replica's replication mode and lag budget are unspecified; Redis AOF is compose-only; NATS file storage has no backup runbook. GDPR delete is transactional and tested, but **disaster-recovery and point-in-time restore are unverified** — the single biggest production-readiness gap for a durable store, and it is entirely independent of the recall feature.

**Bottom line**: the recall feature's persistence design (single-tx state machine, redacted history, GC fencing, aggregate-ordered relay) is sound and well-tested; the two actionable correctness items are F1 (changes_since tie-break) and F2 (message_edits lifecycle), with F3 being the deploy-at-scale gate. No critical findings.
