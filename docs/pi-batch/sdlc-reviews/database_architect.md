All verification complete. Here is the database architect review.

---

# Database Architect Review — Message Recall Time Window (v3 as-built)

**Review basis**: `docs/design/2026-08-06-message-recall-time-window.design.md` (v3), verified against the working tree at `f20e4d1`. Checks that actually ran for this review: 16 storage DB tests + 8 im-core service DB tests + 4 index-fence DB tests on a **fresh throwaway DB migrated with all 238 migrations** (then dropped), 3 aero-common unit + 4 im-core unit recall tests, 5/5 `web/recall_errors.test.js`; schema/SQL/index/wiring inspection via source + live `psql` on the dev cluster (PG17+pgvector, Redis 7, NATS 2.10 all up).

## 1. Store inventory (hot vs durable, stock wiring)

| Store | Class | Purpose (recall-relevant) | Implementation | Stock-binary wiring | Consistency requirement |
|---|---|---|---|---|---|
| **PostgreSQL 17 + pgvector** | **Durable** — source of truth | `messages` (rows, `recalled_at/by`, `version`, `delivery_ordinal`), `message_edits` snapshot, `event_outbox` (recall event), `audit`, `room_members` role | sqlx `PgPool`, migrations compile-time embedded (`aero-storage/db.rs:58`) | `boot/persistence.rs:33` — required, `connect_with_retry`, **migrate runs at every boot**; optional read replica (fail-open). All recall queries use the **primary** pool — no replica-staleness exposure | Strong (fence + outbox append must be atomic; verified) |
| **Redis 7** | **Hot** — cluster state, loss-tolerant | presence, `StreamViewerStore`/`CallRosterStore` sorted sets (sharded), participant cache, login throttle, optional spam-guard backend | fred; `zadd` + TTL eviction | `boot/persistence.rs:71-76` — required at boot (retry); spam-guard Redis backend **opt-in** `AERO_SPAM_GUARD_REDIS`, otherwise in-process DashMap (fail-open). Recall feature touches **no** Redis state | Eventual; reconstructible from heartbeats |
| **NATS JetStream** | **Durable** (`im.room.*`) / **ephemeral** (`live.stream.*`) | room-event fan-out; recall rides `im.room.{id}` with stable `event_id` dedup key | async-nats; file storage, retention 7d (`jetstream.rs:134`) | `boot/persistence.rs` — required, `bootstrap_streams: true`; durable consumer `aero-server` (per-instance hashed names, `bus.rs:131-166`) | At-least-once, ordered per subject via seq |
| **BlobStore** | Durability = backend | attachment bytes freed by recall (GC enqueue in-tx) | LocalFs (node-local) / S3 / aero-vault | `blob_store_from_env_checked`: default `local`; `AERO_BLOB_BACKEND=s3` requires complete config else **fail-loud abort** (`s3_blob_store.rs:490+`) | LocalFs is single-node only; multi-node requires S3 |
| **In-process** (Hub mpsc, rate maps, SfuRouter) | **Hot, ephemeral** | fan-out buffers only | bounded mpsc / DashMap / `Arc<RwLock<HashMap>>` | Always on; rebuilt on restart | None (backed by outbox/NATS) |

**Stock-wiring verdict**: every durable store (PG, NATS, blob) is genuinely wired by the stock binary; Redis is wired as required hot state. No "exists in tree but not wired" hazard found in the recall path. **Verified.**

## 2. Findings (severity-ordered)

### F1 — Medium: migration 0238 rebuilds `idx_messages_room_mutated` non-concurrently on a hot table
- **Evidence**: `migrations/0238_message_recall.sql` — `DROP INDEX IF EXISTS idx_messages_room_mutated;` then `CREATE INDEX IF NOT EXISTS ... ON messages (room_id, GREATEST(edited_at, deleted_at, recalled_at))` (plain, not `CONCURRENTLY`); plus a full `UPDATE messages_partitioned ... FROM messages` join backfill. Verified in the applied 238-migration schema.
- **Impact**: `CREATE INDEX` takes `SHARE` lock — blocks message INSERT/UPDATE/DELETE for the whole index build on a large `messages` table; the DROP→CREATE gap leaves `changes_since` (reconnect backfill, `query.rs:170-181`) without its index (seq scan per room). Dev DB measured at only 746 rows, so this is a scale-time deployment hazard, not a current one.
- **Recommendation**: replace with `CREATE INDEX CONCURRENTLY` under a new name + `DROP INDEX` after (with the documented concurrent-build retry loop); batch the shadow `UPDATE` in the `backfill_messages_partition` id-ordered chunks (which 0238 already reissues with the recall columns) instead of a single join. Required only if `messages` is large at deploy time.

### F2 — Low: app-clock boundary error bound = inter-instance clock skew
- **Evidence**: `created_at` is app-minted at insert (`crud.rs:71`, `now_utc()`); the fence compares it with the **recaller's instance** `now_utc()` (`authorization.rs:261-269`). With the default 86400s window, seconds of NTP skew are negligible; with small operator-set windows (the regression gate uses 1s), the boundary's error bound equals the skew.
- **Impact**: correctness is preserved (fence is atomic under the row lock); only the *boundary instant* is fuzzy across instances. The design already documents the boundary-race fraction as uncounted.
- **Recommendation**: document "window must be ≫ configured clock-skew bound" in `docs/recall-window.md`; optionally add a floor when `0 < secs < skew`. No code change required at default settings.

### F3 — Low: `recalled_by` FK relies on the "participants are never hard-deleted" invariant
- **Evidence**: `0238` adds `recalled_by UUID REFERENCES participants(id)` (NO ACTION); GDPR erasure **tombstones** (`participant.rs` — the erase list is all `DELETE`/`UPDATE`, participants row itself is an UPDATE tombstone), so the FK never fires — identical to the pre-existing `messages.sender_id` FK pattern, and erasure anonymizes by `sender_id` only (`participant.rs:549`), leaving `recalled_by` as an opaque id exactly like `sender_id`.
- **Impact**: none today; consistent with the established pattern. If a future change ever hard-deletes participants, both FKs block — not a recall-specific defect.
- **Recommendation**: none (note for future erasure work).

### F4 — Info: preflight duplicates the authority chain (~8-10 queries per recall attempt)
- **Evidence**: `assert_message_recall_preflight` (`messages.rs:455-527`: `get` + `assert_room_access` 3-5 reads + `role_of`) is re-run inside `recall_message` before the tx fence; REST (`handlers/messages.rs:186-188`) and WS (`frame.rs:163-164`) both preflight **before** `check_ws_rate_room`. All reads are PK-indexed; recall is a cold mutation path, not the send hot path.
- **Impact**: bounded double round-trip per recall; the rate-gate fairness purpose (doomed attempts never burn workspace budget) justifies it. No action.

### F5 — Info (positive): no missing-index hazard
- `lock_message_in_tx` is PK `FOR UPDATE`; `role_of` uses `room_members` PK `(room_id, participant_id)`; the preflight reads are all PK-lookups. No query filters `recalled_at` alone (retention sweeps key on `created_at`/`expires_at`; `changes_since` uses the 0238 expression index). `recall_boundary_race_author_vs_admin` (`tokio::join!`) passed — exactly one winner, one outbox row, stable Conflicts.

## 3. Query/index & transaction analysis (atomic paths)

**Recall tx** (`recall_outboxed_authorized`, `authorization.rs:206-306`) — verified against the running schema:
1. `resolve_message_target` — unlocked identity read (immutable tenant edge).
2. `aero_effective_room_access` (migration 0197) — canonical lock order: workspace `FOR UPDATE` fence → `rooms` `FOR SHARE` → `room_members` `FOR UPDATE`. **Same order as edit/delete/send paths**; no deadlock inversion introduced.
3. `lock_message_in_tx` — `SELECT ... WHERE id=$1 FOR UPDATE` (PK). Window predicate evaluated **on the locked snapshot** (`authorization.rs:261-269`), author-only, appended after role/deleted/recalled gates — atomic by construction.
4. `recall_locked_outboxed_in_tx` — one UPDATE with the **unchanged** terminal fence `WHERE id=$4 AND recalled_at IS NULL AND deleted_at IS NULL`, `version = version + 1`; inserts `message_edits` snapshot (redacted, no byte refs), GC-enqueues original blob ids, writes `message.recalled` audit, appends the `Recalled` outbox event — single transaction, verified by `author_recall_replaces_content_and_records_audit_in_one_tx`.
5. **Outbox ordering**: `insert_idempotent_in_tx` allocates `MAX(aggregate_version)+1` under the message row lock (`event_outbox.rs:200-213`; UNIQUE `(message_id, aggregate_version)`); `claim_due` uses `FOR UPDATE SKIP LOCKED` with per-message and per-subject ordering `NOT EXISTS` guards, supported by `idx_event_outbox_aggregate_pending` (0163) and `idx_event_outbox_message_delivery_pending` (0174). `mark_published` is a CAS on `attempts`; lease 30s; NATS dedup via stable `event_id`. At-least-once holds across the crash windows: commit→fast-dispatch gap is covered by the 250ms batch relay (`background.rs:62-95`); publish-then-crash-before-ack is deduped by NATS message id.
6. **Edit-after-recall** is fenced (`crud.rs:182`: `deleted_at IS NULL AND recalled_at IS NULL AND version = $5`) — the version bump closes the optimistic-lock hole.
7. **Reconnect convergence**: recall does not bump `edited_at`; 0238 reissues the expression index to `GREATEST(edited_at, deleted_at, recalled_at)`; `changes_since_delivers_recalls` (DB test) and the 4 index-fence tests passed.

**Hot path**: the send path is untouched by this feature (window read happens once at `ImService::new`); the metric `aero_messages_recall_expired_total` emits pre-rate-gate (≤20 rps bound, documented).

## 4. Safe migration sequence (compatibility, validation, rollback)

**This feature adds no migration** (238 before/after, verified); the sequence below governs its prerequisite 0238, which is Expand–Migrate–Contract additive. Deployment order matters (AGENTS.md §4.2: **build → migrate**):

1. **Build first** (`cargo build`) — migrations are compile-time embedded; a stale binary silently no-ops new migrations.
2. **Roll-forward (Expand/Migrate)**: run the new binary's boot-time `migrate()`. 0238 is additive: `ADD COLUMN IF NOT EXISTS` (×2 on live + shadow), CHECK drop+add on `event_outbox` (superset of old kinds — old binary's inserts remain valid), index reissue, `CREATE OR REPLACE` backfill function. **Compatibility window**: old binaries run correctly against the new schema (they never read `recalled_at`, never emit `recalled` events).
3. **Validation queries** (run against a throwaway DB first; use fresh-DB replay, never hand-edit `_sqlx_migrations`):
   - `SELECT count(*) FROM _sqlx_migrations;` → 238, no failed rows.
   - `SELECT column_name FROM information_schema.columns WHERE table_name='messages' AND column_name IN ('recalled_at','recalled_by');` → both present.
   - `SELECT conname FROM pg_constraint WHERE conname='event_outbox_kind_check';` and confirm `'recalled'` is in the check.
   - `SELECT indexname FROM pg_indexes WHERE tablename='messages' AND indexname='idx_messages_room_mutated';` and `EXPLAIN` a `changes_since` query → index scan, not seq scan.
   - `SELECT count(*) FROM messages_partitioned shadow LEFT JOIN messages live ON shadow.id=live.id WHERE shadow.recalled_at IS DISTINCT FROM live.recalled_at;` → 0 (shadow reconciled).
4. **Contract**: recall writes begin flowing (outbox kind `recalled`).
5. **Rollback (schema-safe, data-not)**:
   - *Roll-forward* (new binary, migration applied, no recalls yet): revert to old binary + old web bundle — old binary ignores the new columns; new CHECK accepts old kinds; the new expression index is harmless (old `changes_since` predicate falls back to seq scan — the only cost).
   - *After recalls have been applied*: **original content is irrecoverable at the app level** — blocks were replaced with the placeholder, the pre-recall body exists only in `message_edits` (redacted). Recovery requires point-in-time DB restore (or a scripted `message_edits` rewind if that is the product intent). This is inherent to recall (content destruction), but must be stated in the runbook.
6. **Data-integrity checks post-deploy**: `recalled_at IS NOT NULL` ⇒ `blocks` = placeholder JSON, `searchable_text = ''`, `embedding IS NULL`, `version` bumped, exactly one `message_edits` snapshot, one `event_outbox` row with `event_kind='recalled'` and `aggregate_version` contiguous (no gap vs the prior max); `recalled_by IS NULL` must never co-occur with `recalled_at IS NOT NULL` (recall is always by someone). The 16-storage/8-service/4-fence test suite (all green on a fresh 238-migration DB in this review) is the automated form of this.

## 5. Unknown volume, retention, and recovery assumptions

- **Volume**: no production-scale evidence in-tree. Measured dev DB: `messages` 746 rows, `event_outbox` 0, `message_edits` 2, `participants` 1610. The migration-lock finding (F1), `claim_due` scan costs, and `MAX(aggregate_version)+1` contention are all unquantified at scale. Required measurements: `messages` row count + write rate at deploy time; pending `event_outbox` queue depth and `claim_due` p95 latency at ≥1k events/instance-batch; `changes_since` latency for the largest room after 0238.
- **Retention**: `event_outbox` rows are retained indefinitely in the schema (the published partial index suggests a sweep exists or is planned — not verified); NATS `im.room.*` stream retention 7d/30d; audit 365d; the recall window itself (86400s default) is **not** a retention policy — recalled rows persist. The interplay recall→retention soft-delete is verified orthogonal (`recalled_message_can_still_be_deleted` passed).
- **Recovery**: no backup/restore system verified in-tree. Durability story per store: PG (assumed operator-managed backup; PITR needed for recall rollback), LocalFs blob (node-local — single-node loss = attachment loss; S3 required for multi-node durability), Redis (reconstructible), NATS JetStream (7d window covers consumer lag; durable consumers replay past events only within stream retention — a consumer down >7d loses redelivery, and the PG-side ConsumerEventReceipt/side-effect idempotency is the compensating control).
- **Clock skew bound** between app instances is unknown (assumed NTP-synced; see F2).

**Bottom line**: the recall-window feature is persistence-correct as built — the fence is atomic under the row lock, ordering and at-least-once hold through the outbox/NATS pipeline, all DB tests pass on a fresh full-migration DB, and no stock-wiring gap was found. The only actionable item is F1 (non-concurrent index rebuild in 0238) at deploy time for large tables; F2-F4 are documented limitations, not defects.
