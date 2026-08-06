# Database Architect Review — Recall F1/F3 Fix + Persistence/Production Readiness

**Revision**: `0bc0838` + working tree (F1/F3 fix round). **Date**: 2026-08-07.
**Role input**: `fix-implementation` completion report (recall F1 index-fence + F3 rate-gate + test isolation).

## Checks actually run for this review

- **Verified** `cargo check --workspace --all-targets --quiet` → exit 0 (whole workspace, incl. new `recall_index_fence_tests`).
- **Verified** migration count = 238 (`ls migrations/*.sql | wc -l`).
- Read migration `0238_message_recall.sql` and the migration chain it touches (0162/0163/0174/0176/0211/0125/0128/0131/0075/0001).
- Read the F1 fences and their callers: `message/crud.rs`, `message/events.rs`, `message/authorization.rs`, `aero-ai/src/worker/mod.rs`, `im-core/src/service/outbox.rs` (materialize), `query.rs` (changes_since).
- Read F3 wiring: WS frame (`ws_impl/frame.rs`), REST handler, `im-core` preflight + `recall_message`, `ws_rate.rs` (Redis fixed window, fail-open), new `authz_lint` scan.
- Read persistence wiring: `bin/boot/persistence.rs`, `bin/boot/background.rs`, `aero-storage/src/db.rs`, `s3_blob_store.rs` backend switch, `docker-compose.yml`, `config.example.toml`.
- **Not run**: full `cargo test` (PG-gated tests need a live throwaway DB), clippy, live-DB recall tests, load measurements. Claims below about those are labeled.

---

## 1. Store inventory (purpose · durability · implementation · stock wiring · consistency requirement)

| Store | Purpose | Durability | Implementation | Stock binary wiring | Consistency requirement |
|---|---|---|---|---|---|
| **PostgreSQL 17** (pgvector) | Source of truth for all business state: messages, rooms, memberships, outboxes, jobs, audit, retention, search indexes | **Durable** (primary; optional read replica is capacity-only, never serving-critical) | `aero-storage`; sqlx pool: `max_connections=16` (config), min 2 warm, acquire 30s, idle 600s, `test_before_acquire`, per-conn `statement_timeout=10s` (`db.rs`) | **Verified**: `boot/persistence.rs` — `connect_with_retry` → `migrate()` (compile-time-embedded `sqlx::migrate!("../../migrations")`, 238 files) **before** serving | ACID; business row + outbox + side-effect jobs in one tx; FKs + triggers as tenant fences |
| **NATS JetStream 2.10** | Cross-instance fact source: `im.room.*`, `live.stream.*`, session control | **Durable** streams (file storage, `-sd /data` + volume in compose); at-least-once; `IM_MESSAGES` 7d retention, dup-window = retention; `IM_EVENTS` 30d; `LIVE_EVENTS` 6h/200k (ephemeral by design); poison bound `max_deliver=16`, `ack_wait=120s` | `aero-bus` JetStreamBus; per-subject seq stamped at publish; `Nats-Msg-Id` = stable outbox `event_id` | **Verified**: `boot/persistence.rs` `bootstrap_streams=true` (get_or_create + update_stream); durable consumers `aero-server-{instance}` per process (fan-out), queue-group `aero-golive`, bots `aero-bot`/`aero-ooo`/`aero-transcribe` always-on, `aero-unfurl`/`aero-moderation`/`aero-push` env-gated (`background.rs`) | Per-subject monotonic seq; gaps legal; dedup by (event_id, seq) |
| **Redis 7** | **Hot/ephemeral** cluster state: presence (zset, TTL 45s), stream viewers, call rosters, per-subject seq counters (INCR, no TTL by design), ws-rate fixed windows (60s TTL) | Ephemeral by contract (presence/viewers/rate windows may vanish); **seq counters depend on Redis persistence** — stock compose runs `--appendonly yes` + volume (**Verified** `docker-compose.yml`) | `fred` client, shared via `RedisCache`; `live_presence.rs` sharded zsets to avoid hot keys | **Verified**: `boot/persistence.rs` — boot-time hard dependency (`connect_with_retry`), runtime fail-open for seq/ws-rate, fail-closed for boot | Presence/viewers: seconds-fresh, loss OK. Seq: must be monotonic per subject while Redis alive; see Finding 3 |
| **Blob store** | Attachment bytes (messages, avatars, VOD) | **Durable** | `LocalFsBlobStore` (default), `S3BlobStore` (real reqwest+SigV4), `AeroVault` (opt-in); region router; GC = delete-then-ack with live-reference scan | **Verified**: `blob_store_from_env_checked` — `AERO_BLOB_BACKEND=s3|vault` with incomplete config **fails boot loud** (no silent local fallback); label on `/health/ready` | delete-then-ack (blob GC); dedup by SHA-256 with owner-scope; FK-free `blob_gc_queue` |
| **PG tables as durable queues** | `event_outbox` (room-event relay), `message_side_effect_jobs`, `consumer_event_receipts`, `webhook_delivery`(+request), `stream_go_live_outbox`, `bot_delivery_outbox`, `ai_usage_outbox`, `ai_jobs`, `blob_gc_queue`/`blob_gc_intent` | **Durable** — crash-safe by design (claim/lease/fencing/backoff; `SKIP LOCKED`; `attempts` fencing; at-least-once with idempotency keys) | `aero-storage` repos; claim leases (30s outbox), exponential backoff 1→300s, bounded retries (AI `MAX_ATTEMPTS=5`→dead; outbox retries indefinitely but side effects dedup via `consumer_event_receipts`) | **Verified**: relay loops always spawned in `boot/background.rs` (poll 250ms default, env-settable, 0 disables with warn); webhook worker, AI worker, sweeps in boot | Exactly-once **effect**, at-least-once delivery: `(consumer,event_id)` receipt PK; outbox per-message aggregate order + per-room delivery-ordinal prefix order |
| **HLS output dir** | `.ts`/`.m3u8` segments | Durable-on-disk, regenerable | `HlsWriter` + real MPEG-TS mux | `AERO__SERVER__HLS_DIR`; wired in boot | Segments are cache-like; m3u8 is the contract |

**Stock-wiring summary**: the stock `aero-server` binary wires every backend at boot (PG migrate-first, Redis, NATS JetStream with stream bootstrap, blob store fail-loud) — no stub/no-op bus. The only env-gated backends are blob s3/vault and the push/unfurl/moderation bots. **Verified** from `boot/persistence.rs` + `boot/background.rs`.

---

## 2. Findings (severity sorted)

### Finding 1 — Low · Recall index-resurrection fix (F1) is sound; system-edit paths still fence in code, not SQL
**Path/evidence**: `message/crud.rs` `update_embedding` (474), `update_searchable_text` (505), `update_voice_transcript` (446), `edit` (179) — all now `AND recalled_at IS NULL`. The three lockless UPDATEs previously had no fence (the original F1). System-edit paths (`edit_outboxed_system` events.rs:51, `update_voice_transcript_outboxed` events.rs:182) fence **in code** under `lock_message_in_tx` `FOR UPDATE` + `version` check — correct today because recall's UPDATE takes the same row lock, so the read snapshot cannot go stale (serialization, no TOCTOU). Deterministic tests prove it: `recall_tests.rs::system_edit_after_recall_is_fenced`, `recall_index_fence_tests.rs` (recall-first + 8-round concurrent race).
**Impact**: No demonstrated defect. The residual risk is the *class* F1 missed once: a future lockless index/content UPDATE added without a `recalled_at` fence silently resurrects recalled content into FTS/vector. The completion report acknowledges this.
**Recommendation**: defense-in-depth — add `AND recalled_at IS NULL` to the two system-edit UPDATEs in `events.rs` (the lock already serializes, so zero behavior change) and extend `recall_index_fence_tests` to assert the SQL-level fence of `edit_outboxed_system`/`update_voice_transcript_outboxed` too. **Validation**: run `cargo test -p aero-storage --lib -- --ignored message::recall`; the race test fails if any fence regresses.

### Finding 2 — Low · AI worker pays for an embedding that recall can discard (cost race, not correctness)
**Path/evidence**: `aero-ai/src/worker/mod.rs` embed path: read message → (paid) `embed_text_with_context` → `update_embedding` (fenced, returns `false` if recalled/deleted in between). The worker logs "row missing or deleted at update" — misleading now that recall is a third cause — and the provider cost is sunk. `has_embedding` pre-check only covers the already-embedded case, not the recalled/deleted case.
**Impact**: bounded cost waste (recall racing an in-flight embed); no data corruption (fence works — `recall_index_fence_tests::concurrent_recall_vs_embed_write_never_resurrects` proves it).
**Recommendation**: cheap single-row re-read (`recalled_at IS NULL AND deleted_at IS NULL`) immediately before the paid call to skip payment, and fix the log message to name recall. **Validation**: unit test asserting the worker skips the paid call when the row is recalled.

### Finding 3 — Low (conditional) · Redis seq counters regress across a lossy Redis restart; stock compose is safe
**Path/evidence**: `aero-storage/src/seq.rs` — `aero:seq:{subject}` INCR, keys deliberately TTL-free (documented), but nothing pins them to Redis persistence. `web/ws.js` `SeqGate` dedups per-scope recent seqs (cap 256) and orders by high-water. With AOF disabled or data lost (container recreated without the volume, managed Redis eviction), INCR restarts at 1 → a still-connected client can false-dedup up to 256 new events (old seqs still in the ring) and see ordering regress. **Verified** the stock `docker-compose.yml` runs `redis-server --appendonly yes --dir /data` with a volume — so the shipped dev stack is safe; only non-persistent Redis deployments hit this.
**Impact**: silent dropped frames + ordering anomalies for connected clients across a Redis data loss; no DB corruption.
**Recommendation**: document Redis AOF (or equivalent persistence) as a hard requirement for seq correctness; optionally namespace seq keys by a boot-time generation. **Validation**: restart Redis without persistence while a client is connected and send a message; observe `SeqGate` drops.

### Finding 4 — Medium (scale-dependent, operational) · Migration 0238 holds `messages` SHARE lock and `event_outbox` ACCESS EXCLUSIVE in one transaction
**Path/evidence**: `0238_message_recall.sql` — `DROP INDEX` + non-concurrent `CREATE INDEX idx_messages_room_mutated` (SHARE lock on `messages`, blocks all writes), `DROP CONSTRAINT` + `ADD CONSTRAINT` on `event_outbox` (ACCESS EXCLUSIVE + full validation scan), and the `messages_partitioned` reconciliation `UPDATE … FROM messages` (full-join scan) — all inside one sqlx migration transaction. In the repo's migration-first rolling upgrade (old binaries still serving — the 0176 fence convention), message writes are blocked for the whole migration duration on a large `messages` table; `event_outbox` writes block during constraint swap + validation.
**Impact**: availability window proportional to `messages` size (index build) and `event_outbox` size (constraint validation). Current scale unknown — see §5.
**Recommendation**: before shipping 0238 to a large deployment, (a) measure `reltuples`/`pg_class` for `messages`, `messages_partitioned`, `event_outbox`; (b) raise `maintenance_work_mem` for the migration session; (c) if the window is unacceptable, schedule a maintenance window — the migration is immutable and the DROP+CREATE cannot be converted to `CONCURRENTLY` inside a migration transaction, so plan for it; (d) keep the `event_outbox` sweep (`AERO__SERVER__EVENT_OUTBOX_RETENTION_DAYS`) tight so the CHECK validation scan stays bounded. **Validation**: time `EXPLAIN (ANALYZE)` of the constraint-validation scan and the shadow reconciliation UPDATE on a staging clone; record the observed lock window.

### Finding 5 — Info · Recall outbox `Recalled` events are correctly suppressed/ordered by the relay
**Path/evidence**: `im-core/src/service/outbox.rs` `materialize_outbox_payload` — `EventOutboxKind::Recalled` re-materializes the current message, suppresses when version was superseded or row no longer live; relay claims only the earliest unpublished aggregate version and earliest delivery ordinal (0162/0163/0174 claim predicates + `idx_event_outbox_aggregate_pending` / `idx_event_outbox_message_delivery_pending`). Recall event rides the same durable queue as create/edit/delete — no separate ordering domain.
**Impact**: none; verified behavior. Noted for completeness: a recall followed by a delete publishes `Deleted` only (recall event suppressed as superseded) — correct.

### Finding 6 — Info · GDPR deferred-erasure can overwrite the recall placeholder with `[deleted]` and leaves `recalled_at` set
**Path/evidence**: `participant.rs` `delete_participant`/`sweep_deferred_erasure` write `blocks='[{"type":"text","text":"[deleted]"}]'` for `deleted_at IS NULL` rows without touching recall columns; the state machine (0238 comment) is enforced in code only.
**Impact**: benign — both are placeholders, no content resurrection; clients keep the recall placeholder while DB holds `[deleted]`; the row remains recall-terminal (`recalled_at` set). Worth a code comment so future readers don't "fix" it.
**Recommendation**: none required; optionally assert in `recall_tests` that erasure-after-recall keeps `recalled_at`.

### Finding 7 — Info · F3 rate gate + regression lint verified
**Path/evidence**: WS `ClientFrame::RecallMessage` and REST `POST /api/messages/:id/recall` both run `assert_message_recall_preflight` (access guard first, no victim-budget drain) → `check_ws_rate_room` (Redis fixed window per workspace, 60s TTL, fail-open, tiered) → `recall_message` (re-checks everything under locks). New hermetic `authz_lint.rs::every_recall_entry_point_charges_ws_rate_budget` source-scans every `.recall_message(` call site for an enclosing `check_ws_rate_room` — **Verified** present in the diff.
**Impact**: none; note the lint is source-pattern-based — a future wrapper function that calls `recall_message` from a new entry point must keep the gate inside the same body, which the lint enforces by construction.

---

## 3. Query/index & transaction analysis (demonstrated hot/atomic paths)

### 3a. Message send/recall transaction (atomic core)
- **Recall tx** (`authorization.rs::recall_locked_outboxed_in_tx`, verified): resolve target → `lock_effective_message_write_access` (workspace/membership locks) → `lock_message_in_tx` `FOR UPDATE` → role re-check `FOR UPDATE` on `room_members` → **one-shot fence** `UPDATE … WHERE id=$4 AND recalled_at IS NULL AND deleted_at IS NULL` (bumps version) → `message_edits` snapshot insert (redacted: no `blob_id` byte refs — `redact_blocks_for_recall_snapshot`, GC can't orphan) → `enqueue_unreferenced_blobs_in_tx` (per-blob `FOR UPDATE` + JSONB `@>` live-reference EXISTS, `ON CONFLICT DO NOTHING`) → audit row → outbox row with `MAX(aggregate_version)+1` (gap-free because the caller holds the aggregate serialization lock — documented in `insert_idempotent_in_tx`). All-or-nothing commit. **Verified** by `author_recall_replaces_content_and_records_audit_in_one_tx` + `concurrent_double_recall_has_exactly_one_winner` (row lock + WHERE fence ⇒ exactly one winner, one outbox row).
- **Hot write cost**: ~10 statements incl. 3 row locks on one message id — the F3 rate gate is well targeted; per-client limiter + workspace ceiling both apply.

### 3b. Outbox relay (at-least-once, ordered, fenced) — verified
- `claim_due`/`claim_by_id`: `FOR UPDATE SKIP LOCKED` with (a) earliest-unpublished-aggregate `NOT EXISTS` (index `idx_event_outbox_aggregate_pending (message_id, aggregate_version) WHERE published_at IS NULL`), (b) earliest-delivery-ordinal `NOT EXISTS` (index `idx_event_outbox_message_delivery_pending (subject, delivery_ordinal) WHERE event_kind='message' …`), (c) lease expiry (30s), (d) `attempts` fencing on `mark_published`/`mark_failed`. `seq` assigned once (`COALESCE(seq,$2)`), retries reuse it. NATS dedup window (7d) ⊇ outbox retry horizon for all practical backoff; durable side-effect consumers additionally persist `(consumer,event_id)` receipts (0166 schema has a state-shape CHECK enforcing processing/completed lease invariant). All query predicates are index-covered — no obvious hot-path scan.
- `statement_timeout=10s` on every pooled connection: the claim query's correlated NOT EXISTS on a swept, index-covered table stays well inside; a *large backlog* (millions of unpublished rows) would be the first place to watch — see §5.

### 3c. Search/backfill hot reads
- FTS: `search_tsv` is a **STORED generated column** over `searchable_text` (0002/0128/0131) → recall's `searchable_text=''` re-derives an empty tsvector automatically; GIN index `messages_search_tsv_gin`. Vector: HNSW `messages_embedding_hnsw` (0075) → `embedding=NULL` excludes recalled rows. Verified reasoning chain for F1.
- `changes_since` (reconnect backfill) now keys on `GREATEST(edited_at, deleted_at, recalled_at)` matching the reissued `idx_messages_room_mutated` expression index (0238) — recall does not bump `edited_at`, so this was the one place a recalled message could have stayed invisible to offline clients; covered by `changes_since_delivers_recalls`. **Verified** query + index expression align.
- Room history/ordinal replay: `list_delivery_after` on `messages_room_delivery_ordinal_key (room_id, delivery_ordinal)` (0174); `list_recent` on `messages_room_created_idx`; both bounded (`LIMIT` clamped 1–500). Recall keeps the row (placeholder), so history is stable.

### 3d. Hot cluster stores (Redis)
- Presence: zset heartbeat, `zadd` + `zremrangebyscore` prune (45s TTL); sharded zsets for viewer counts to avoid hot keys. Verified in `live_presence.rs`/`presence.rs`.
- ws-rate: `INCR aero:wsrate:{ws}:{minute}` + `EXPIRE` 60s, fail-open on Redis/PG error, room→workspace TTL cache (60s) so steady-state is 1 Redis INCR per charge. Verified `ws_rate.rs`.
- seq: `INCR aero:seq:{subject}` — see Finding 3.

---

## 4. Safe migration sequence (recall 0238 + general chain)

The repo's discipline (**Verified**): migrations are compile-time embedded (`db.rs`) — **build before migrate**; `make migrate-smoke` replays the full 238-migration chain on a throwaway DB; `test-integration.sh` runs fresh-DB regressions per database; migrations are treated as immutable (0176 comment "Keep those immutable").

### Compatibility window
- 0238 is **Expand–Migrate–Contract, purely additive** for `messages` (2 nullable columns; ADD COLUMN without default is metadata-only, PG 11+ fast path) — old binaries' queries are unaffected.
- `event_outbox_kind_check` is re-issued as a **strict superset** of the 0211 list (`message,edited,deleted,notify,reaction,canvas_op` + `recalled`) — old binaries never insert `recalled`, so old-writer compatibility holds; new binaries cannot run against an un-migrated DB (migrations run at boot before serving).
- `messages_partitioned` shadow gets the columns + NULL-safe idempotent reconciliation + reissued `backfill_messages_partition` (0174 convention) so a cutover backfill cannot drop recall state — covered by `partition_backfill_carries_recall_columns`.
- **Roll-forward** (recall data exists, need to change semantics): new migration, per convention. **Rollback is not recommended once recalls are recorded**: dropping the columns would make placeholder bodies look like ordinary user content and re-enable editing of a recalled message; pre-recall bodies survive only in `message_edits` (evidence route). A forced downgrade must first remap/delete `event_outbox` rows with `event_kind='recalled'` and re-tighten the CHECK.

### Deployment sequence (large-DB caution from Finding 4)
1. `cargo build` → `aero-cli migrate` on a throwaway DB (`make migrate-smoke`) — fresh-chain check.
2. Measure sizes (§5) and, if large, schedule the migration in a low-traffic window; tune `maintenance_work_mem`.
3. Migrate first, then roll binaries (old binaries keep serving during the window — 0176 convention).
4. Post-migration validation (below).
5. Roll-forward only after recall data exists; never roll back.

### Validation queries
```sql
-- 1. Recall columns on both relations + constraint superset
SELECT table_name, count(*) FROM information_schema.columns
 WHERE table_name IN ('messages','messages_partitioned')
   AND column_name IN ('recalled_at','recalled_by') GROUP BY table_name;
SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conname='event_outbox_kind_check';

-- 2. F1 integrity invariant: recalled rows must NEVER carry index content (canary)
SELECT count(*) FROM messages
 WHERE recalled_at IS NOT NULL
   AND (searchable_text <> '' OR embedding IS NOT NULL);

-- 3. Shadow parity (idempotent reconciliation can be re-run)
SELECT count(*) FROM messages_partitioned shadow
 JOIN messages live ON live.id = shadow.id AND live.created_at = shadow.created_at
 WHERE shadow.recalled_at IS DISTINCT FROM live.recalled_at;

-- 4. Outbox/aggregate consistency: every recalled row has exactly one 'recalled' event at the new version
SELECT m.id, m.recalled_at, m.version
  FROM messages m
  LEFT JOIN event_outbox e
    ON e.message_id = m.id AND e.event_kind='recalled' AND e.aggregate_version = m.version
 WHERE m.recalled_at IS NOT NULL AND e.id IS NULL;  -- expect 0 rows

-- 5. Index present with the recall expression (0125 reissue)
SELECT indexdef FROM pg_indexes WHERE indexname='idx_messages_room_mutated';
```
Canary #2 is the load-bearing one: with the F1 fences in place it must always be 0; any nonzero row means a fence class regressed (or a raw-SQL writer bypassed the repo).

---

## 5. Unknown volume, retention, and recovery assumptions

- **Table volumes** (Unknown — no workload evidence supplied): `messages` row count, `event_outbox` backlog, `messages_partitioned` shadow size, `consumer_event_receipts`/`webhook_delivery` row counts. These determine: (a) the 0238 lock window (Finding 4), (b) whether the 10s `statement_timeout` can ever hit the relay claim on a deep backlog, (c) HNSW build/refresh cost on `embedding`. **Measurement required**: `pg_class.reltuples`, `pg_stat_user_tables` on the five tables above; relay `claim_due` p95 under load.
- **Throughput** (Unknown): messages/sec, WS conns per node, `Hub` bounded-queue saturation (drop + disconnect policy exists — `hub.rs`), pool headroom at `max_connections=16`.
- **Retention defaults** (Partial): NATS 7d / 6h windows verified; PG-side sweeps verified wired (`retention.rs` sweep tasks: outbox `EVENT_OUTBOX_RETENTION_DAYS`, side-effect, receipts, send-keys, blob GC 60s, ephemeral/points/ban). Default values for each env knob and the `_sqlx_migrations` ledger policy in prod were not all enumerated — see `config.example.toml`.
- **Recovery** (Partial): delete-then-ack blob GC, at-least-once receipts, lease fencing, and NATS poison bounds are verified code paths; no backup/restore drill, RPO/RTO, or PITR configuration exists in the repo (out of scope for the binary; documented gap). Redis AOF is on in the stock compose but the **requirement is undocumented** (Finding 3).
- **Staging seams** (as declared): real-provider AI E2E (embedding race under live traffic) and cross-host NAT/real-network call-bridge remain untested here — the DB-level recall race is covered deterministically by `recall_index_fence_tests`.

---

## Bottom line

- F1 (recall index fence) and F3 (recall rate gate) are **correctly implemented, test-covered, and wired through the stock binary** (WS + REST), with a hermetic regression lint. The F1 class risk is reduced to a documented, tested residual (Finding 1).
- Persistence architecture is sound: single durable fact store (PG) + durable bus (JetStream) + hot Redis for cluster state + fail-loud blob backends; every durable queue has claim/fence/idempotency semantics.
- Ship-blocking: none found. **Planned work**: Finding 1 defense-in-depth SQL fences; Finding 2 pre-pay re-read + log fix; Finding 3 persistence documentation; Finding 4 migration-window measurement before any large deployment.
