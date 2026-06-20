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
> That file is the exact, end-to-end **verified** cutover (parity gate + the 8 FK
> rewrites + index rebuild + table swap + rollback section). It is a **docs
> artifact — NOT a migration**: never put it in `migrations/`, never let the
> deploy chain run it. Execute it BY HAND, by a DBA, in the approved window, with
> writes gated and a tested backup. Run it with `-v ON_ERROR_STOP=1` so any error
> aborts the single transaction and leaves the live `messages` untouched. The
> verification of that exact script is recorded in **§5a** below.

The script's shape (read the file for the fully-commented form):
```sql
BEGIN;
LOCK TABLE messages IN ACCESS EXCLUSIVE MODE;
-- 1. Final catch-up loop (backfill_messages_partition until 0) + a FULL-COLUMN
--    final sync that ALSO copies mls_group_id/mls_epoch/mls_payload — the shipped
--    backfill function OMITS those 3 MLS columns (see ⚠️ below) — then a hard
--    row-parity gate that RAISEs and aborts if messages ≠ messages_partitioned.
-- 2a. Rename the old table's indexes aside (`__old` suffix): index names are
--     SCHEMA-GLOBAL, so the canonical names must be freed before 2b.
-- 2b. Build all 11 messages indexes on the shadow parent (GIN search_tsv,
--     searchable_text trgm, blocks GIN, HNSW embedding, room_created, room_mutated,
--     room_active_id, reply_to, mls_group, expires_at, sender_id_active).
-- 3. Re-add the outbound FKs (room_id→rooms, sender_id→participants).
-- 4. Self-ref reply_to: add reply_to_created_at, backfill, composite self-FK to (id,created_at).
-- 5. Repoint the 7 inbound child FKs — each: DROP old (message_id) FK, ADD
--    msg_created_at column, backfill from the new parent, SET NOT NULL, ADD
--    composite FK (message_id, msg_created_at) → messages(id, created_at)
--    ON DELETE CASCADE  (reactions, notifications, pins, bookmarks,
--    message_receipts, block_interactions, notification_bundles).
-- 6. ALTER TABLE messages RENAME TO messages_old; messages_partitioned RENAME TO messages;
COMMIT;
```

**FK strategy = composite FK (option A).** Each child gains a redundant
`msg_created_at` column so its FK can reference the full composite PK
`(id, created_at)`; this **preserves the DB-enforced `ON DELETE CASCADE`** every
child relies on (no triggers, no app change). The trade-off vs. option B
(trigger-enforced, no real FK) is spelled out in the script header. The self-ref
uses a `reply_to_created_at` column for the same reason.

> ⚠️ **MLS-column gap (important).** The shipped `backfill_messages_partition`
> function (migration `0148`) lists only 12 columns and **omits `mls_group_id`,
> `mls_epoch`, `mls_payload`**. The pre-window backfill loop therefore does NOT
> carry MLS payloads. The cutover script's in-window **full-column** final sync
> (Step 1) copies all 15 non-generated columns including MLS, so no encrypted
> payload is lost — but do NOT rely on the backfill loop alone for MLS rows.

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
  partition and none unexpectedly fell into `messages_part_default`.
- **FK integrity**: for each child table, every `message_id` still resolves —
  `SELECT count(*) FROM child c LEFT JOIN messages m ON m.id=c.message_id WHERE
  m.id IS NULL;` must be 0. Confirm the FKs exist and are `VALIDATED`
  (`SELECT conname, convalidated FROM pg_constraint WHERE confrelid='messages'::regclass`).
- **Indexes present & used**: `\d+ messages` shows the HNSW/GIN/FTS/partial
  indexes on the parent; run a representative semantic-search and a room-history
  query and check `EXPLAIN` prunes partitions and uses the indexes.
- **App smoke**: post a message, react, pin, bookmark, mark-read, get a
  notification — every FK-CASCADE path — then soft-delete and confirm cascade.

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

---

## 6. Rollback

- **Before cutover (Steps A–B):** no rollback needed — the live `messages` table
  was never touched. Just `DROP TABLE messages_partitioned CASCADE;`.
- **At/after cutover (Step C):** the cutover is one transaction — if it fails it
  `ROLLBACK`s and the original `messages` is intact (it was only `RENAME`d at the
  very end inside the txn). If a problem surfaces *after* `COMMIT` but before
  `DROP TABLE messages_old`, reverse the renames inside a new locking txn
  (`messages` → `messages_partitioned`, `messages_old` → `messages`) and recreate the
  original `(id)` FKs; writes since cutover live in `messages_partitioned` (now renamed
  back) and must be replayed into `messages_old` — script this replay as part of
  window prep, do not improvise it.
- **Ultimate fallback:** restore the pre-window backup. This loses any writes
  since the backup, so it is the last resort and is why writes are paused at
  cutover and the soak period precedes `DROP messages_old`.

---

## 7. Risks

- **Doubled disk** during migration (shadow copy). Verify headroom first.
- **Long HNSW/FTS index builds** dominate the window; size `maintenance_work_mem`
  and the window accordingly.
- **FK rewrite touches 7 relationships across 6 tables** — any missed child FK
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
