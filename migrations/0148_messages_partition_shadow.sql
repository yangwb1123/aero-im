-- 0148_messages_partition_shadow.sql
-- ===========================================================================
-- ADDITIVE prep for partitioning `messages` — the SHADOW TABLE + BACKFILL/
-- MAINTENANCE functions from docs/runbooks/messages-partitioning.md Steps A–B.
--
-- ⚠️ HARD-STOP CONTEXT: actually partitioning the LIVE `messages` table is a
--    destructive, maintenance-window operation (PK rewrite + full-table copy +
--    7 inbound-FK rewrites across 6 child tables + pgvector/FTS index rebuilds)
--    and is DELIBERATELY NOT in the auto-applied chain. See the runbook.
--
-- WHAT THIS MIGRATION DOES (and ONLY this):
--   * Creates a NEW, EMPTY partitioned table `messages_partitioned`, a sibling
--     of `messages`. It is PARTITION BY RANGE (created_at) with the composite
--     PK (id, created_at) the partition-key rule demands.
--   * Creates a DEFAULT catch-all partition + a few initial monthly partitions.
--   * Creates `backfill_messages_partition(batch_size, from_id)` so ops can
--     incrementally copy live rows into the shadow table BEFORE the window.
--   * Creates `ensure_messages_partitions(...)` to pre-create future monthly
--     partitions (the maintenance function the runbook references).
--
-- WHAT THIS MIGRATION DELIBERATELY DOES *NOT* DO (all left to the window):
--   * It NEVER touches `messages`: no RENAME, no DROP, no ALTER, no new index,
--     no trigger. The live hot read/write path is byte-for-byte unchanged, so
--     this file is safe to ship in the normal chain and `make migrate-smoke`
--     exercises it on a fresh DB.
--   * NO dual-write. Wiring the message-insert hot path to also write the shadow
--     table is a correctness/latency risk (double-insert in the request path,
--     partial-failure semantics, generated-column recompute) and is intentionally
--     avoided. The shadow table is kept in sync via the backfill function + a
--     final in-window catch-up, exactly as the runbook prescribes.
--   * NO cutover. Repointing the 7 child FKs and the RENAME swap is the
--     destructive part and stays in the runbook's Step C maintenance window.
--   * NO HNSW/GIN/FTS indexes on the shadow table yet — building them once,
--     AFTER the bulk backfill, is far cheaper than maintaining them per inserted
--     row during backfill (runbook Step B). The window procedure adds them.
--
-- IDEMPOTENT: every object is created with IF NOT EXISTS / CREATE OR REPLACE, so
--   a re-run (or a fresh-DB chain replay) is a clean no-op.
-- ===========================================================================

-- ---------------------------------------------------------------------------
-- 1) The shadow partitioned parent.
--
-- `LIKE messages INCLUDING DEFAULTS INCLUDING GENERATED` copies every column
-- (types + NOT NULLs + column DEFAULTs) AND carries the STORED generated column
-- `search_tsv` forward as a generated column — matching the runbook's Step A
-- exactly. It intentionally does NOT pull in indexes or the (id)-only PK; we
-- declare the composite (id, created_at) PK the partition key requires.
--
-- It also does NOT copy the outbound FKs (room_id→rooms, sender_id→participants,
-- reply_to self-ref). That is deliberate: this is an offline STAGING table being
-- bulk-loaded out of order; re-declaring/validating those FKs (and the inbound
-- ones) is part of the in-window cutover, not of this additive prep.
CREATE TABLE IF NOT EXISTS messages_partitioned (
    LIKE messages INCLUDING DEFAULTS INCLUDING GENERATED,
    -- Partition-key rule: every UNIQUE/PK must contain the partition key.
    -- (id, created_at) keeps existing `WHERE id = …` point lookups index-friendly
    -- (id leads) while satisfying the rule — the runbook's chosen key order.
    PRIMARY KEY (id, created_at)
) PARTITION BY RANGE (created_at);

-- DEFAULT catch-all partition: a row whose created_at has no explicit monthly
-- partition still lands somewhere (never a "no partition found" failure), so the
-- backfill and any later sync can't be blocked by a missing month.
CREATE TABLE IF NOT EXISTS messages_partitioned_default
    PARTITION OF messages_partitioned DEFAULT;

-- A few initial monthly partitions around "now" so the common case routes out of
-- DEFAULT immediately. ensure_messages_partitions() (below) rolls the rest; the
-- backfill of older history lands in DEFAULT unless ops pre-creates more months.
DO $seed$
DECLARE
    m         date := date_trunc('month', now() AT TIME ZONE 'UTC')::date;
    lo        date;
    part_name text;
BEGIN
    -- [this month - 1 .. this month + 3] => 5 monthly partitions.
    FOR lo IN
        SELECT generate_series(m - interval '1 month', m + interval '3 months',
                               interval '1 month')::date
    LOOP
        part_name := 'messages_partitioned_' || to_char(lo, 'YYYYMM');
        IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = part_name) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF messages_partitioned '
                || 'FOR VALUES FROM (%L) TO (%L)',
                part_name, lo, (lo + interval '1 month')::date
            );
        END IF;
    END LOOP;
END
$seed$;

-- ---------------------------------------------------------------------------
-- 2) Incremental backfill function (runbook Step B).
--
-- Copies up to `batch_size` live rows from `messages` into `messages_partitioned`
-- in ascending `id` order, starting strictly AFTER `from_id` (the high-water
-- mark the caller threads through). `ON CONFLICT DO NOTHING` makes it idempotent
-- and resumable: re-running a batch, or overlapping batches, never errors.
--
-- It selects `*` so the column set tracks `messages` automatically (the shadow
-- table has the identical column set via LIKE; `search_tsv` is generated on both
-- sides, but is read-and-rewritten transparently by `INSERT … SELECT *` because
-- a plain SELECT * does NOT project generated columns into the INSERT target —
-- PG recomputes them on the shadow side from the copied `searchable_text`).
--
-- Returns one row: (rows_copied, last_id) — the number of source rows scanned in
-- this batch and the new high-water mark to pass as `from_id` next call. When
-- rows_copied = 0 the backfill has caught up to the current tail of `messages`.
--
-- This holds NO lock on `messages` beyond the brief MVCC snapshot of a batched
-- read; it is meant to be called in a throttled loop OUTSIDE the window so the
-- in-window catch-up is tiny. It is READ-ONLY w.r.t. `messages` (SELECT only).
CREATE OR REPLACE FUNCTION backfill_messages_partition(
    batch_size INT  DEFAULT 5000,            -- rows per call (throttle knob)
    from_id    UUID DEFAULT '00000000-0000-0000-0000-000000000000'
) RETURNS TABLE (rows_copied BIGINT, last_id UUID)
LANGUAGE plpgsql
AS $fn$
-- (no DECLARE block: a single RETURN QUERY computes both outputs)
BEGIN
    -- One statement: the data-modifying `ins` CTE runs the INSERT and the final
    -- SELECT … INTO reports both the count of rows ACTUALLY inserted (ins) and the
    -- new high-water mark (max id seen in this batch, regardless of conflicts).
    -- A data-modifying CTE is referenced by the outer query here, so PG executes
    -- it exactly once as part of this statement.
    RETURN QUERY
    WITH batch AS (
        SELECT m.*
          FROM messages m
         WHERE m.id > from_id
         ORDER BY m.id
         LIMIT batch_size
    ),
    ins AS (
        -- List the source columns explicitly so the generated `search_tsv` is
        -- NOT fed from the source and is instead recomputed on insert. Every
        -- other column (incl. searchable_text it derives from) is carried over.
        INSERT INTO messages_partitioned (
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text, expires_at
        )
        SELECT
            id, room_id, sender_id, blocks, reply_to, metadata, embedding,
            created_at, edited_at, deleted_at, searchable_text, expires_at
          FROM batch
        ON CONFLICT DO NOTHING
        RETURNING id
    )
    SELECT
        (SELECT count(*) FROM ins)::BIGINT,    -- rows ACTUALLY inserted this call
        -- New high-water mark = largest id in the batch (the id-ordered tail).
        -- Use the batch (not ins) so conflicting rows still advance the cursor.
        -- `ORDER BY id DESC LIMIT 1` avoids needing a max(uuid) aggregate (PG has
        -- none). Empty batch ⇒ keep the input cursor so the caller sees "caught up".
        COALESCE((SELECT id FROM batch ORDER BY id DESC LIMIT 1), from_id);
END
$fn$;

COMMENT ON FUNCTION backfill_messages_partition(INT, UUID) IS
    'ADDITIVE prep: incrementally copy messages → messages_partitioned in id-ordered '
    'batches (ON CONFLICT DO NOTHING). Call in a throttled loop before the cutover '
    'window; returns (rows_copied, last_id). 0 rows_copied = caught up. Reads messages '
    'only — never writes/locks the live table. See docs/runbooks/messages-partitioning.md.';

-- ---------------------------------------------------------------------------
-- 3) Partition maintenance — pre-create future MONTHLY partitions.
--
-- Monthly (not daily) partitions are recommended for messages: a daily-partitioned
-- multi-year history is thousands of partitions and degrades the planner. This
-- function ensures partitions exist for [this month - 1 .. this month + ahead]
-- so an insert always has a non-DEFAULT home. It is idempotent and safe to
-- over-call. It NEVER drops anything (messages has retention policy + the future
-- cutover owns retention; this prep does not delete message history).
CREATE OR REPLACE FUNCTION ensure_messages_partitions(
    ahead INT DEFAULT 3   -- create this many months of future partitions
) RETURNS void
LANGUAGE plpgsql
AS $fn$
DECLARE
    m         date := date_trunc('month', now() AT TIME ZONE 'UTC')::date;
    lo        date;
    part_name text;
BEGIN
    -- Only act once the shadow parent exists (it always does post-0148, but this
    -- keeps the function harmless if invoked in an odd order).
    IF NOT EXISTS (
        SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relname = 'messages_partitioned' AND n.nspname = 'public'
           AND c.relkind = 'p'
    ) THEN
        RETURN;
    END IF;

    FOR lo IN
        SELECT generate_series(m - interval '1 month',
                               m + (ahead || ' months')::interval,
                               interval '1 month')::date
    LOOP
        part_name := 'messages_partitioned_' || to_char(lo, 'YYYYMM');
        IF NOT EXISTS (SELECT 1 FROM pg_class WHERE relname = part_name) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF messages_partitioned '
                || 'FOR VALUES FROM (%L) TO (%L)',
                part_name, lo, (lo + interval '1 month')::date
            );
        END IF;
    END LOOP;
END
$fn$;

COMMENT ON FUNCTION ensure_messages_partitions(INT) IS
    'ADDITIVE prep: pre-create monthly partitions of the shadow messages_partitioned '
    'table for [this month - 1 .. this month + ahead]. Idempotent; never drops. Run '
    'before/around the cutover window so inserts route out of the DEFAULT partition.';
