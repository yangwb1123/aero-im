# Runbook — Partitioning the `messages` table (maintenance-window, NOT in the chain)

> **STATUS: hard-STOP — do NOT add a destructive `messages` migration to the
> migration chain.** Partitioning `messages` is an offline, maintenance-window
> operation that requires explicit product/ops sign-off. This document is the
> approved procedure to execute *during a scheduled window*; it is intentionally
> **not** wired into `migrations/` and must never be run by the normal deploy.
>
> Contrast with `stream_viewer_samples`, which *was* safely partitioned online in
> `migrations/0144_stream_viewer_samples_partition.sql` precisely because it has
> **zero** foreign keys and a throwaway surrogate PK. `messages` is the opposite.

---

## 0. What is already pre-built (migration `0148`) vs. what stays in the window

The **non-destructive, additive** prep — Steps A and B's standing objects — has
been shipped in `migrations/0148_messages_partition_shadow.sql` and is in the
normal chain (it is proven by `make migrate-smoke`). It **never touches the live
`messages` table** (no `RENAME`/`DROP`/`ALTER`/new index/trigger on it), so the
hot read/write path is byte-for-byte unchanged. Pre-built objects:

| Object | What it is | Built by |
|--------|------------|----------|
| `messages_partitioned` | the empty partitioned **shadow** parent — `LIKE messages INCLUDING DEFAULTS INCLUDING GENERATED`, `PARTITION BY RANGE (created_at)`, composite PK `(id, created_at)`. Coexists with `messages`. | `0148` |
| `messages_partitioned_default` + 5 initial **monthly** partitions | DEFAULT catch-all so an insert never fails, plus `[this month-1 .. +3]`. | `0148` |
| `backfill_messages_partition(batch_size, from_id)` | incremental, **idempotent** (`ON CONFLICT DO NOTHING`), id-ordered batch copy `messages → messages_partitioned`; returns `(rows_copied, last_id)`. Read-only on `messages` — no lock, **not** a dual-write. | `0148` |
| `ensure_messages_partitions(ahead)` | pre-create future monthly partitions (idempotent; never drops). | `0148` |

`0148` deliberately does **NOT**:
- **dual-write** — wiring the message-insert hot path to also write the shadow
  table is a latency + partial-failure + generated-column-recompute risk. The
  shadow stays in sync via the backfill loop + a short in-window final catch-up.
- **cutover** — the FK repointing across 7 inbound FKs and the table swap are the
  destructive part; they remain Step C below, inside the maintenance window.
- build the **HNSW/GIN/FTS** indexes on the shadow yet — building them once after
  the bulk backfill (Step B) is far cheaper than maintaining them per inserted row.

**So the operator flow shrinks to:** (1) drive `backfill_messages_partition` in a
throttled loop until `rows_copied = 0`, days/hours ahead of the window (Step B);
then (2) inside the window do the short final sync + build indexes + cutover
(Step C). Steps A and the standing functions are already done.

A thin storage wrapper exists too:
`MessageRepo::backfill_messages_partition(batch_size, from_id) -> (rows_copied,
last_id)` (`crates/aero-storage/src/message/sweep.rs`) for driving the loop from
an admin/ops path.

---

## 1. Why `messages` is a hard-STOP

PostgreSQL declarative partitioning has two hard rules that collide with the
`messages` schema:

1. **The partition key must be part of every UNIQUE / PRIMARY KEY constraint.**
   `messages` PK today is `PRIMARY KEY (id)` (verified:
   `pg_get_constraintdef` → `PRIMARY KEY (id)`). To partition by `created_at` the
   PK must become `(id, created_at)` (or `(created_at, id)`). Changing the PK is a
   schema rewrite.

2. **You cannot turn an existing populated table into a partitioned table in
   place.** A partitioned parent must be `CREATE TABLE … PARTITION BY …` from the
   start. So the live `messages` table must be *recreated* and every row copied —
   a full-table rewrite of the single largest table in the system.

On top of those two PG rules, `messages` is referenced by **7 foreign keys from 6
other tables plus a self-reference** (verified on the live catalog —
`confrelid='messages'::regclass`, `contype='f'`):

| Referencing table     | FK column      | On delete   | Source migration |
|-----------------------|----------------|-------------|------------------|
| `messages` (self)     | `reply_to`     | (no action) | `0001_init.sql`  |
| `notifications`       | `message_id`   | CASCADE     | `0010_notifications.sql` |
| `pins`                | `message_id`   | CASCADE     | `0011_pins.sql`  |
| `bookmarks`           | `message_id`   | CASCADE     | `0020_bookmarks.sql` |
| `message_receipts`    | `message_id`   | CASCADE     | `0077_message_receipts.sql` |
| `block_interactions`  | `message_id`   | CASCADE     | `0086_block_interactions.sql` |
| `reactions`           | `message_id`   | CASCADE     | `0002_p2_collab_ai.sql` |
| `notification_bundles`| `message_id`   | CASCADE     | `0143_notification_bundles.sql` |

> A FK can only target a UNIQUE/PK constraint. Once the `messages` PK changes
> from `(id)` to `(id, created_at)`, **every one of these FKs breaks** — a FK on
> `(message_id)` can no longer be satisfied by a PK on `(id, created_at)` unless
> the child also carries `created_at` and references `(id, created_at)`. So each
> child table must be altered (drop FK, optionally add a `created_at`/partition
> column, recreate FK against the new key) — and several of these children
> (`reactions`, `notifications`) are themselves large.

Two further `messages`-specific complications:

- **`embedding vector(1024)` + an HNSW index** (`messages_embedding_hnsw`,
  pgvector). HNSW index builds are slow and memory-hungry; rebuilding it on a
  multi-million-row copied table dominates the window.
- **`search_tsv` is a `GENERATED ALWAYS … STORED` column** (FTS). It is
  recomputed on every inserted row, so a bulk `INSERT … SELECT` backfill pays the
  `to_tsvector` cost per row.

**Net:** partitioning `messages` = PK change + full-table rewrite + 7 FK
rewrites on child tables + pgvector/FTS index rebuilds. That is a destructive,
locking, multi-table migration that must run in a maintenance window with a
tested rollback. **Never put it in the auto-applied chain.**

---

## 2. Recommended path (online-ish, shadow table + backfill + cutover)

This is the lowest-downtime approach and the recommended one. It keeps the old
table serving reads/writes while a new partitioned table is built and backfilled,
then cuts over inside a short lock window. Plan for hours of background backfill
and a **short** (seconds–minutes) exclusive window at cutover.

### Prerequisites
- Maintenance window approved; on-call + DBA present.
- A **tested restore** of a recent backup (this is your ultimate rollback).
- Disk headroom for a *second full copy* of `messages` (the shadow table) plus
  index space — partitioning temporarily doubles the table's footprint.
- Chosen retention: daily vs. monthly partitions. **Monthly is recommended** for
  `messages` (a daily-partitioned multi-year history is thousands of partitions;
  the planner and `\d+` degrade). Use daily only with a short retention.

### Step A — Build the partitioned shadow parent (no lock on live table)
> **ALREADY DONE by migration `0148`** (see §0). The shadow parent is named
> `messages_partitioned` (not `messages_part`); the DEFAULT partition and the
> initial monthly partitions already exist, and `ensure_messages_partitions()`
> rolls future months. Run it once before the window to widen coverage:
> ```sql
> SELECT ensure_messages_partitions(12);  -- ensure ~a year of months exist
> ```
> The original hand-rolled DDL is kept below for reference / understanding only —
> you do **not** re-run it:
```sql
-- (REFERENCE ONLY — 0148 already created the equivalent as `messages_partitioned`.)
CREATE TABLE messages_partitioned (
    LIKE messages INCLUDING DEFAULTS INCLUDING GENERATED,
    -- PK must include the partition key:
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);
CREATE TABLE messages_partitioned_default PARTITION OF messages_partitioned DEFAULT;
CREATE TABLE messages_partitioned_202606 PARTITION OF messages_partitioned
    FOR VALUES FROM ('2026-06-01') TO ('2026-07-01');
-- … repeat per month (or call ensure_messages_partitions(); or pg_partman, §3).
```
> `INCLUDING GENERATED` carries `search_tsv` forward as a generated column.
> Do **not** create the HNSW / GIN / FTS indexes yet — add them *after* the bulk
> backfill so each is built once, not maintained per inserted row.

### Step B — Backfill in batches (live table stays online)
> **Use the pre-built function** `backfill_messages_partition(batch_size, from_id)`
> (migration `0148`) instead of hand-rolling the copy. It copies one id-ordered,
> resumable, idempotent (`ON CONFLICT DO NOTHING`) batch and returns
> `(rows_copied, last_id)`; it only `SELECT`s from `messages` (no lock, no
> dual-write). Drive it from a throttled ops loop, threading `last_id` back as
> `from_id`, until `rows_copied = 0` (caught up). E.g. in psql:
> ```sql
> -- one batch; repeat with the returned last_id as the next from_id:
> SELECT * FROM backfill_messages_partition(5000, '00000000-0000-0000-0000-000000000000');
> ```
> or via storage: `MessageRepo::backfill_messages_partition(5000, from_id)` in a
> loop with a sleep between calls. The original raw form is kept below for
> reference:
```sql
-- (REFERENCE ONLY — prefer backfill_messages_partition() above.)
INSERT INTO messages_partitioned
SELECT * FROM messages
WHERE created_at >= $batch_lo AND created_at < $batch_hi
ON CONFLICT DO NOTHING;
```
Track the high-water mark; repeat until caught up to "now". Then build indexes on
the parent (cascades to partitions) — the HNSW build is the long pole:
```sql
CREATE INDEX … ON messages_partitioned (room_id, GREATEST(edited_at, deleted_at));
CREATE INDEX … ON messages_partitioned USING gin (blocks);
CREATE INDEX … ON messages_partitioned USING hnsw (embedding vector_cosine_ops)
    WITH (m='16', ef_construction='128');
-- plus the expires_at partial index and any others from \d messages.
```

### Step C — Cutover (the short exclusive window)

> **▶ USE THE VERIFIED SCRIPT: [`docs/runbooks/messages-cutover.sql`](./messages-cutover.sql).**
> That file is the exact, end-to-end verified cutover procedure (parity gate +
> the 8 FK rewrites + index rebuild + table swap + rollback section). Its
> current revision was most recently rehearsed against the chain through
> `0176_rolling_upgrade_fences.sql` on 2026-07-28; evidence is recorded in
> **§5d**. It is a **docs
> artifact — NOT a migration**: never put it in `migrations/`, never let the
> deploy chain run it. Execute it BY HAND, by a DBA, in the approved window, with
> writes gated and a tested backup. Run it with `-v ON_ERROR_STOP=1` so any error
> aborts the single transaction and leaves the live `messages` untouched. The
> verification of that exact script and its rollback is recorded in **§5d** below.

The script's shape (read the file for the fully-commented form):
```sql
BEGIN;
LOCK TABLE messages IN ACCESS EXCLUSIVE MODE;
-- 1. Final catch-up loop (backfill_messages_partition until 0) + a full
--    mutable-column sync including MLS, version, and delivery_ordinal, then hard row
--    parity + ordinal-equality gates that RAISE and abort on drift.
-- 2a. Rename the old table's indexes aside (`__old` suffix): index names are
--     SCHEMA-GLOBAL, so the canonical names must be freed before 2b.
-- 2b. Build/rename all 12 messages indexes on the shadow parent (GIN search_tsv,
--     searchable_text trgm, blocks GIN, HNSW embedding, room_created, room_mutated,
--     room_active_id, room_delivery_ordinal, reply_to, mls_group, expires_at,
--     sender_id_active).
-- 3. Re-add the outbound FKs (room_id→rooms, sender_id→participants).
-- 4. Self-ref reply_to: add reply_to_created_at, backfill, composite self-FK to (id,created_at).
-- 5. Repoint the 7 inbound child FKs — each: DROP old (message_id) FK, ADD
--    msg_created_at column, backfill from the new parent, SET NOT NULL, ADD
--    composite FK (message_id, msg_created_at) → messages(id, created_at)
--    ON DELETE CASCADE  (reactions, notifications, pins, bookmarks,
--    message_receipts, block_interactions, notification_bundles).
-- 6. ALTER TABLE messages RENAME TO messages_old; messages_partitioned RENAME TO messages;
-- 7. Add permanent BEFORE triggers that resolve child msg_created_at and
--    reply_to_created_at from the canonical parent for deployed writers.
-- 8. Replace ensure_messages_partitions() with a relation-aware implementation
--    that maintains canonical messages after the swap and the parked parent
--    after a rollback.
COMMIT;
```

**FK strategy = composite FK (option A).** Each child gains a redundant
`msg_created_at` column so its FK can reference the full composite PK
`(id, created_at)`; this **preserves the DB-enforced `ON DELETE CASCADE`** every
child relies on. A permanent fill trigger keeps the stable application write
contract (`message_id` only) compatible; the trigger supplies only the redundant
key half and the real composite FK remains the integrity authority. The self-ref
uses `reply_to_created_at` plus the same fill pattern and `MATCH FULL`, so a
caller cannot bypass it by nulling only the redundant key half.

> **Schema mirror invariant.** Migration `0174` reissues
> `backfill_messages_partition` with every current non-generated message column,
> including MLS, `version`, and `delivery_ordinal`, and mirrors the ordinal
> constraint/index/trigger onto `messages_partitioned`. `LIKE` remains a one-time
> snapshot: every future column added to live `messages` must update the shadow,
> backfill projection, final sync, and §5 verification in the same change.

> ⚠️ **Older-than-(this month − 1) history needs partitions pre-created.**
> `ensure_messages_partitions(ahead)` only creates `[this month − 1 .. + ahead]`.
> Any backfilled rows older than last month fall into `messages_partitioned_default`
> (functionally correct, but they aren't pruned by month). For a clean layout,
> pre-create the historical monthly partitions before backfill (loop
> `CREATE TABLE … PARTITION OF messages_partitioned FOR VALUES FROM … TO …` over
> your true history range), or accept the DEFAULT partition as their home.

Resume the application. Verify (see §5) before dropping `messages_old`.

### Step D — Decommission
After a soak period and a verified backup of the new table, `DROP TABLE
messages_old;` to reclaim the doubled disk. Add the recurring
partition-maintenance job (§3).

---

## 3. Alternatives

- **`pg_partman`** — automates partition *creation* (premake future partitions)
  and *retention* (drop/detach old). It does **not** do the initial in-place
  conversion or the FK rewrite for you, but once `messages` is partitioned it
  removes the need to hand-roll the maintenance cron. Recommended for ongoing
  ops if the team is comfortable adding the extension.

- **PK change to `(created_at, id)` first, FKs handled explicitly** — a variant
  of §2 where the *partition column order* is `(created_at, id)` to make
  range-pruning the leading key. Same FK-rewrite cost; choose key order based on
  whether point lookups by `id` (favor `(id, created_at)`) or time-range scans
  (favor `(created_at, id)`) dominate. `messages` does both; `(id, created_at)`
  keeps existing `WHERE id = …` lookups index-friendly and is the §2 default.

- **`DETACH`/`ATTACH` instead of copy** — if you can afford to first add the
  composite PK to the *existing* table and rename it into a single initial
  partition, you can `ATTACH` it under a new parent without copying every row.
  This avoids the full rewrite but still requires the PK change (which itself
  rewrites the table to add `created_at` to the unique index) and the FK rewrite,
  so the savings are partial and the procedure is fiddlier. Use only with DBA
  sign-off.

- **`pg_partman` + logical-replication cutover** — for the very largest
  installs, build the partitioned copy on a replica via logical replication and
  cut over with near-zero downtime. Highest operational complexity; only if the
  maintenance window in §2 is unacceptable.

---

## 4. Maintenance-window requirements

- **Approved window** with product + ops sign-off (this is destructive).
- **Backup taken and a restore tested** immediately before starting.
- **Disk headroom** for a second full copy of `messages` + its indexes.
- **`maintenance_work_mem`** raised for the index builds (esp. HNSW).
- **Application writes paused or gated** for the Step C cutover transaction.
- **Monitoring**: replication lag (if any), lock waits, disk, the backfill
  high-water mark.
- A **partition-maintenance job** scheduled *before* go-live so the day/month
  after cutover already has its partition (or rely on the DEFAULT partition as
  the safety net, exactly as `0144` does for `stream_viewer_samples`).

---

## 5. Verification

After cutover, before dropping `messages_old`:

- **Row parity**: `SELECT count(*) FROM messages;` equals the pre-cutover count
  of `messages_old` (account for rows written during the final catch-up).
- **Partition routing**: spot-check that recent rows landed in the right monthly
  partition and none unexpectedly fell into `messages_partitioned_default`.
- **FK integrity**: for each child table, every `message_id` still resolves —
  `SELECT count(*) FROM child c LEFT JOIN messages m ON m.id=c.message_id WHERE
  m.id IS NULL;` must be 0. Confirm the FKs exist and are `VALIDATED`
  (`SELECT conname, convalidated FROM pg_constraint WHERE confrelid='messages'::regclass`).
- **Indexes present & used**: `\d+ messages` shows the HNSW/GIN/FTS/partial
  indexes on the parent; run a representative semantic-search and a room-history
  query and check `EXPLAIN` prunes partitions and uses the indexes.
- **App smoke**: post a message, react, pin, bookmark, mark-read, get a
  notification — every FK-CASCADE path — then hard-delete and confirm cascade.

---

## 5a. Recorded verification of `messages-cutover.sql` (throwaway DB, 2026-06-19)

The exact `docs/runbooks/messages-cutover.sql` was run end-to-end on a **brand-new
throwaway PostgreSQL** (`pgvector/pgvector:pg17`, db `aero_cutover_verify`, on an
isolated container — **NOT** the shared `aero` DB). Procedure: replay all **148**
`migrations/*.sql` (incl. `0148`) on a fresh DB → seed 4 messages spanning 3
months + 1 reply (cross-partition self-ref) + one row in **each** of the 7 child
tables → run `backfill_messages_partition` to `rows_copied = 0` → run
`messages-cutover.sql` (committed clean) → verify. Results:

- **(a) Row parity** — `new_messages = 4`, `messages_old = 4` (equal). After the
  swap, `pg_class.relkind` for `messages` = `p` (partitioned), `messages_old` = `r`.
- **(b) 8 inbound FKs present & VALIDATED** — exactly 8 distinct constraint names
  (`reactions_message_id_fkey`, `notifications_message_id_fkey`,
  `pins_message_id_fkey`, `bookmarks_message_id_fkey`,
  `message_receipts_message_id_fkey`, `block_interactions_message_id_fkey`,
  `notification_bundles_message_id_fkey`, plus the self-ref
  `messages_reply_to_fkey`), all `convalidated = t`. *(In `pg_constraint` the
  self-ref name recurs once per partition — a normal partitioned-table catalog
  artifact, not a duplicate constraint.)*
- **(b) Referential integrity** — the LEFT-JOIN orphan check returned **0** for
  all 7 children.
- **(b) `ON DELETE CASCADE` preserved** — hard-deleting the 2-months-ago message
  (each child held 1 referencing row) cascaded all 7 children to **0** rows,
  proving the composite-FK rewrite kept the app-relied-upon cascade semantics.
- **(c) FTS** — `search_tsv @@ to_tsquery('english','charlie')` returns the row;
  accent-insensitive `to_tsquery('english','cafe')` still matches `café` (the
  `english` stemmer + `f_unaccent` generated expression survived the copy).
- **(c) Room-history keyset** — `WHERE room_id = … ORDER BY created_at DESC LIMIT`
  returns rows newest-first; `EXPLAIN` shows partition pruning
  (`Subplans Removed`) and uses the per-partition `(room_id, created_at DESC)`
  index.
- **(c) `reply_to` self-ref** — the reply (in `…_202606`) resolves to its target
  (in `…_default`) across partitions via the composite self-FK.
- **(d) Partition routing** — `tableoid::regclass` per row confirmed messages
  landed in `messages_partitioned_202606` / `_202605` and the older one in
  `messages_partitioned_default` (older than `ensure_messages_partitions`'s
  `[this month−1 …]` floor — see the §C historical-partition note).

The throwaway database **and its container were dropped** after verification; the
shared `aero` DB and the `migrations/` chain were never touched.

## 5b. Re-verification after the `version` schema-drift fix (throwaway DB, 2026-07-16)

Migration `0157` added a `version` column to `messages` (optimistic-lock edit
counter) **after** `0148` had already run `LIKE messages INCLUDING DEFAULTS` to
build `messages_partitioned` — `LIKE` is a one-time snapshot, not a live mirror,
so the shadow table and `backfill_messages_partition` silently never carried
`version`. Left as found, a real cutover would have reset every previously-edited
message's version to the column default (1), permanently breaking optimistic-lock
checks for any client that remembered a higher version. Found by rescanning this
runbook's own June record against the current schema, *before* attempting a real
cutover. Fixed in `migrations/0158_messages_partition_shadow_version_column.sql`
(adds the column to the shadow + reissues `backfill_messages_partition` to carry
it) and `messages-cutover.sql`'s final-sync (now reconciles `version` alongside
the existing MLS columns).

Re-ran the full §5a procedure on a fresh throwaway DB (`pgvector/pgvector:pg17`,
isolated container, **not** the shared `aero` DB) against the then-current full
migration chain, with one addition: message A was seeded at `version = 5` (simulating 4
prior edits) instead of the column default, specifically to exercise the fix.
Procedure: replay the complete chain → seed 4 messages (A at version 5, spanning
3 months) + 1 reply + one row in each of the 7 child tables → `ensure_messages_partitions(12)`
→ `backfill_messages_partition` to `rows_copied = 0` → run the updated
`messages-cutover.sql` → verify. Results:

- **`version` preserved through backfill** — `messages_partitioned` showed
  `version = 5` for message A immediately after the (fixed) backfill function
  ran, not the column default of 1.
- **`version` preserved through cutover** — after the swap, `SELECT id, version
  FROM messages` still showed `version = 5` for message A. This is the specific
  regression the fix targets, and it held.
- **Every §5a check re-confirmed on the current chain**: row parity (4 = 4),
  `messages` relkind = `p` / `messages_old` = `r`, all 8 inbound FK constraints
  present and `convalidated = t`, 0 orphans across all 7 children, FTS match on
  `search_tsv`, partition pruning present in `EXPLAIN` (`Append` over per-month
  partitions), cascade delete verified (deleted the reply first — `reply_to` is
  `NO ACTION`, not `CASCADE`, unchanged from the pre-partition schema and
  correctly still enforced — then deleted message A and confirmed all 7 children
  dropped to 0).

The throwaway database was dropped after verification; the shared `aero` DB and
the `migrations/` chain were never touched. **The cutover script's version-column
handling was verified** against the schema as of migration 0158 — a future column addition to
`messages` will reintroduce the same class of gap unless this runbook and
`messages-cutover.sql`'s final-sync column list are updated alongside it.

## 5c. Re-verification for `delivery_ordinal` (throwaway DB, 2026-07-28)

Migration `0174` adds a transactional per-room `delivery_ordinal`. Because
`messages_partitioned` was snapshotted by migration 0148, 0174 explicitly:

- adds/backfills the shadow column and applies its NOT NULL/positive invariant;
- reissues `backfill_messages_partition` with MLS, version, and ordinal;
- builds the partition-compatible unique index
  `(room_id, delivery_ordinal, created_at)`;
- attaches the NULL-only ordinal-allocation trigger to both live and shadow
  parents, so explicit backfill ordinals are preserved while post-cutover
  application inserts still allocate from `room_delivery_sequences`;
- updates the final sync and parity gate in `messages-cutover.sql`.

The full isolated rehearsal was repeated against the then-current complete chain
embedded in a freshly rebuilt `aero-cli`. A separate 0173→0174 upgrade
fixture seeded an unsafe legacy MAX-ULID cursor, pre-existing shadow rows, three
message outbox rows, and a soft-deleted message; 0174 was then executed
repeatedly. The full-chain fixture seeded MLS/version values plus an intentionally
wrong pre-existing shadow ordinal before running the current cutover script.
Results:

- **Conservative upgrade:** the legacy cursor retained its diagnostic
  `last_seq = 99` but migrated to `last_delivery_ordinal = 0`; the unsafe old
  message id was never certified as a contiguous prefix.
- **Idempotency:** re-executing the original 0174 SQL after assignment reported
  zero message/shadow/cursor updates; live ordinals remained exactly `1,2,3`,
  and each named constraint/index remained singular.
- **Shadow fidelity:** the reissued batch backfill preserved
  `delivery_ordinal = 2`, `version = 6`, `mls_epoch = 8`, and MLS payload
  `cafe`. Final sync corrected the deliberately stale shadow ordinal `99` to
  live ordinal `1`.
- **Cutover parity:** the full-chain rehearsal committed with new/old row counts
  `3 = 3`, zero ordinal mismatches, zero invalid or duplicate room ordinals,
  and zero `room_delivery_sequences` coverage gaps.
- **Post-cutover allocation:** exactly one enabled
  `messages_assign_delivery_ordinal` parent trigger remained; an application
  insert omitting the column received the next room ordinal `4`.
- **Replay safety/performance:** the deleted-secret fixture produced zero
  visible replay rows; all three delivery-cursor PG tests and both outbox
  ordering/retry PG tests passed. With sequential scans disabled, `EXPLAIN`
  selected each partition's `(room_id, delivery_ordinal, created_at)` index for
  the ordinal replay query.

Both purpose-built databases were dropped after verification; no shared
development or final-validation database was used.

## 5d. Re-verification of write compatibility and exact rollback (throwaway DB, 2026-07-28)

The current chain through `0176_rolling_upgrade_fences.sql` was replayed on a
fresh PostgreSQL 17 + pgvector database with a freshly rebuilt `aero-cli`.
The fixture covered a cross-partition reply, every inbound child relation, a
workspace-scoped blob, delivery ordinals, and rows later inserted, edited, and
hard-deleted during the post-cutover soak. The exact current
`messages-cutover.sql` and its exact companion
`messages-cutover-rollback.sql` were then executed without hand edits.

Results:

- **Stable child write contract:** post-cutover inserts into reactions,
  notifications, pins, bookmarks, message receipts, block interactions, and
  notification bundles all omitted `msg_created_at`; the permanent triggers
  resolved the real parent key and all composite FKs validated.
- **Self-reference cannot be weakened:** the self-FK reported `MATCH FULL`.
  Directly setting `reply_to_created_at = NULL` was repaired to the target's
  timestamp, and deleting a still-referenced target failed with SQLSTATE
  `23503`.
- **Partition maintenance follows the relation:** calls before and after the
  swap both increased the appropriate partitioned parent's child count. After
  an exact rollback, the same function maintained the parked
  `messages_partitioned` parent rather than the restored heap.
- **Blob tenancy survives the rename:** the `0176` blob-scope trigger remained
  attached to the promoted parent and rejected a cross-workspace attachment
  with SQLSTATE `23514`.
- **Rollback replays the soak:** messages inserted after cutover were present in
  the restored heap, an edit remained visible, and a hard-deleted row remained
  absent. The rollback parity gate found no differing application-visible row.
- **Original catalog shape restored:** all seven child relations returned to
  their message-id-only FK and no longer exposed `msg_created_at`; together with
  the heap self-reference, all eight inbound FKs were validated. Every canonical
  message index and the PK belonged to the restored heap, while the failed
  partitioned copy retained only its parked names.
- **Post-rollback application probes:** message, reply, and all seven child
  writes succeeded without redundant columns; delivery ordinals continued
  monotonically, hard-delete cascade removed every child, and the blob
  workspace boundary remained enforced.

The throwaway database was force-dropped after verification. No shared
development or final-validation database was used.

---

## 6. Rollback

- **Before cutover (Steps A–B):** no rollback needed — the live `messages` table
  was never touched. Just `DROP TABLE messages_partitioned CASCADE;`.
- **At/after cutover (Step C):** the cutover is one transaction — if it fails it
  `ROLLBACK`s and the original `messages` is intact (it was only `RENAME`d at the
  very end inside the txn). If a problem surfaces *after* `COMMIT` but before
  `DROP TABLE messages_old`, keep writes gated and execute the verified companion
  [`messages-cutover-rollback.sql`](./messages-cutover-rollback.sql). It replays
  post-cutover inserts/updates/hard-deletes, restores all seven original child
  FKs and every canonical index/PK name, reverses the table swap, and runs
  catalog gates in one transaction.
- **Ultimate fallback:** restore the pre-window backup. This loses any writes
  since the backup, so it is the last resort and is why writes are paused at
  cutover and the soak period precedes `DROP messages_old`.

---

## 7. Risks

- **Doubled disk** during migration (shadow copy). Verify headroom first.
- **Long HNSW/FTS index builds** dominate the window; size `maintenance_work_mem`
  and the window accordingly.
- **FK rewrite touches 7 relationships across 7 tables** — any missed child FK
  silently loses referential integrity; the §5 LEFT JOIN check is mandatory.
- **Partition explosion** if daily partitions are chosen for multi-year history —
  use monthly + retention.
- **Lock contention** at cutover — keep the Step C transaction minimal; the bulk
  backfill (Step B) is what runs long, and it holds no lock on the live table.

---

### See also
- `migrations/0144_stream_viewer_samples_partition.sql` — the *safe* counterpart:
  online partition conversion of a FK-free, surrogate-PK firehose, including the
  reusable DEFAULT-partition safety net and the
  `ensure_stream_viewer_sample_partitions()` maintenance function pattern.
- `scripts/migrate_chain_smoke.sh` / `make migrate-smoke` — fresh-deploy chain
  replay. The `messages` partitioning above is deliberately **excluded** from the
  chain, so this guard continues to prove the auto-applied path stays clean.
