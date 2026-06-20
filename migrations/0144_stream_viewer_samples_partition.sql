-- Convert stream_viewer_samples into a PG declarative RANGE partition by day.
--
-- WHY THIS IS SAFE (and messages is NOT — see docs/runbooks/messages-partitioning.md):
--   stream_viewer_samples is the highest-cardinality append-only table in the
--   schema (a 30s viewer-count firehose, migration 0072). Crucially it has:
--     * NO inbound foreign keys  — nothing REFERENCES it
--       (grep 'REFERENCES stream_viewer_samples' = 0; pg_constraint confirms 0
--        rows with confrelid = 'stream_viewer_samples'::regclass), and
--     * NO outbound foreign key   — stream_id is deliberately FK-less so a sample
--       survives a hard-delete of its stream (see migration 0072 comment).
--   So no FK has to be dropped/recreated and no other table is rewritten. Reads
--   and writes only ever key on (stream_id, sampled_at) — the surrogate `id` is a
--   PK placeholder that no query ever filters on — so widening the PK to include
--   the partition key is invisible to the application.
--
-- PARTITION KEY: sampled_at (RANGE), one partition per UTC day. Daily partitions
--   make retention a metadata-only `DROP PARTITION` instead of a big bulk DELETE
--   (the rollup_and_downsample sweep keeps working unchanged on the parent; the
--   per-partition DROP is an *additional*, cheaper path documented at the bottom).
--
-- PARTITION-KEY RULE: every UNIQUE/PRIMARY KEY on a partitioned table must contain
--   the partition key, so the PK becomes (id, sampled_at) instead of (id). `id` is
--   a gen_random_uuid() surrogate that is globally unique anyway, and no query
--   filters by id alone, so (id, sampled_at) is a strictly looser constraint with
--   no behavioural change.
--
-- SAFE SEQUENCE (single transaction — sqlx wraps each migration file in one):
--   1. RENAME the existing table out of the way.
--   2. CREATE the new partitioned parent with the same columns + (id, sampled_at) PK.
--   3. Create a DEFAULT partition (catch-all) + today's and tomorrow's daily
--      partitions, so inserts ALWAYS have a home even if partition maintenance lags.
--   4. Copy every legacy row into the parent (routed to the right partition).
--   5. DROP the legacy table.
--   6. Recreate the (stream_id, sampled_at) lookup index on the parent (it cascades
--      to every partition automatically).
--
-- IDEMPOTENT-ISH: this file is a one-way structural change. It is guarded so that
--   re-running it on an already-partitioned table is a no-op (the rename target
--   would not exist / the parent would already be partitioned), via the DO block.

DO $migrate$
BEGIN
    -- Only run the conversion when stream_viewer_samples is still a plain
    -- (non-partitioned) table. If it is already partitioned (this migration
    -- already applied), skip the whole thing so a re-run is a clean no-op.
    IF EXISTS (
        SELECT 1
          FROM pg_class c
          JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relname = 'stream_viewer_samples'
           AND n.nspname = 'public'
           AND c.relkind = 'r'            -- ordinary table, NOT 'p' (partitioned)
    ) THEN

        -- 1) Move the current table aside.
        ALTER TABLE stream_viewer_samples RENAME TO stream_viewer_samples_legacy;
        ALTER INDEX stream_viewer_samples_pkey
            RENAME TO stream_viewer_samples_legacy_pkey;
        -- The old lookup index name is freed for reuse on the new parent.
        ALTER INDEX stream_viewer_samples_stream_sampled_idx
            RENAME TO stream_viewer_samples_legacy_stream_sampled_idx;

        -- 2) New partitioned parent. Same column types/defaults as 0072; the PK
        --    gains sampled_at to satisfy the partition-key rule.
        CREATE TABLE stream_viewer_samples (
            id          UUID        NOT NULL DEFAULT gen_random_uuid(),
            stream_id   UUID        NOT NULL,
            sampled_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            viewers     INT         NOT NULL,
            PRIMARY KEY (id, sampled_at)
        ) PARTITION BY RANGE (sampled_at);

        -- 3a) DEFAULT partition: the catch-all so an INSERT whose sampled_at has no
        --     explicit daily partition still succeeds (never a "no partition" error
        --     if maintenance lags). Rows here can later be redistributed, but more
        --     importantly nothing ever fails to insert.
        CREATE TABLE stream_viewer_samples_default
            PARTITION OF stream_viewer_samples DEFAULT;

        -- 3b) Today's and tomorrow's daily partitions (UTC). The sweep below /
        --     scripts/partition maintenance creates the rolling future ones.
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF stream_viewer_samples '
            || 'FOR VALUES FROM (%L) TO (%L)',
            'stream_viewer_samples_' || to_char((now() AT TIME ZONE 'UTC')::date, 'YYYYMMDD'),
            (now() AT TIME ZONE 'UTC')::date,
            ((now() AT TIME ZONE 'UTC')::date + 1)
        );
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF stream_viewer_samples '
            || 'FOR VALUES FROM (%L) TO (%L)',
            'stream_viewer_samples_' || to_char(((now() AT TIME ZONE 'UTC')::date + 1), 'YYYYMMDD'),
            ((now() AT TIME ZONE 'UTC')::date + 1),
            ((now() AT TIME ZONE 'UTC')::date + 2)
        );

        -- 4) Copy legacy rows into the parent (router sends each to its partition;
        --    any row whose day has no daily partition lands in DEFAULT).
        INSERT INTO stream_viewer_samples (id, stream_id, sampled_at, viewers)
            SELECT id, stream_id, sampled_at, viewers FROM stream_viewer_samples_legacy;

        -- 5) Drop the legacy table.
        DROP TABLE stream_viewer_samples_legacy;

        -- 6) Recreate the lookup index on the parent (cascades to all partitions).
        CREATE INDEX stream_viewer_samples_stream_sampled_idx
            ON stream_viewer_samples (stream_id, sampled_at);

    END IF;
END
$migrate$;

-- ---------------------------------------------------------------------------
-- PARTITION MAINTENANCE
--
-- A daily-partitioned table needs (a) future partitions created before rows
-- arrive for that day, and (b) old partitions dropped past the raw-retention
-- window. ensure_stream_viewer_sample_partitions() does both in one idempotent
-- call; the retention sweep (boot/retention.rs) invokes it each cycle so no
-- external cron is required, and the catch-all DEFAULT partition guarantees an
-- insert never fails even between maintenance runs.
--
-- It creates partitions for [today-1 .. today+ahead] (so a clock just past
-- midnight, or a slightly-backdated sample, always has a daily home) and drops
-- daily partitions strictly older than `keep_days`. It NEVER touches the DEFAULT
-- partition and NEVER drops a partition that could hold rows inside the retention
-- window, so it is safe to over-call.
CREATE OR REPLACE FUNCTION ensure_stream_viewer_sample_partitions(
    keep_days INT DEFAULT 14,   -- drop daily partitions older than this
    ahead     INT DEFAULT 3     -- create this many days of future partitions
) RETURNS void
LANGUAGE plpgsql
AS $fn$
DECLARE
    d            date;
    part_name    text;
    today        date := (now() AT TIME ZONE 'UTC')::date;
    drop_before  date := (now() AT TIME ZONE 'UTC')::date - keep_days;
    r            record;
BEGIN
    -- (a) Ensure a partition exists for yesterday .. today+ahead.
    FOR d IN
        SELECT generate_series(today - 1, today + ahead, interval '1 day')::date
    LOOP
        part_name := 'stream_viewer_samples_' || to_char(d, 'YYYYMMDD');
        IF NOT EXISTS (
            SELECT 1 FROM pg_class WHERE relname = part_name
        ) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF stream_viewer_samples '
                || 'FOR VALUES FROM (%L) TO (%L)',
                part_name, d, d + 1
            );
        END IF;
    END LOOP;

    -- (b) Drop daily partitions entirely older than the retention window. Only
    --     partitions named stream_viewer_samples_YYYYMMDD are considered; the
    --     DEFAULT partition (no date suffix) is never matched, so it survives.
    FOR r IN
        SELECT c.relname
          FROM pg_inherits i
          JOIN pg_class c     ON c.oid = i.inhrelid
          JOIN pg_class p     ON p.oid = i.inhparent
         WHERE p.relname = 'stream_viewer_samples'
           AND c.relname ~ '^stream_viewer_samples_[0-9]{8}$'
    LOOP
        -- Parse the YYYYMMDD suffix back into a date; drop if its whole day is
        -- strictly before the retention cutoff.
        IF to_date(right(r.relname, 8), 'YYYYMMDD') < drop_before THEN
            EXECUTE format('DROP TABLE IF EXISTS %I', r.relname);
        END IF;
    END LOOP;
END
$fn$;
