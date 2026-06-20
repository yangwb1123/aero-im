-- 0146_audit_events_partition.sql
-- Convert audit_events into a PG declarative RANGE partition by day, reusing the
-- safe pattern proven in migration 0144 (stream_viewer_samples). This is the
-- "方向四 无界 append-only 日志堆" follow-up: bound the audit trail's growth and
-- make retention a metadata-only DROP PARTITION instead of a bulk DELETE.
--
-- ==========================================================================
-- PER-TABLE EVALUATION (why audit_events is safe and webhook_delivery_log is NOT)
-- ==========================================================================
--
-- audit_events  → PARTITIONED (this file)
--   * NO inbound foreign keys — `grep -rn "REFERENCES audit_events" migrations/`
--     returns nothing; nothing else REFERENCES this table, so widening its PK to
--     include the partition key rewrites no dependent constraint.
--   * GENUINELY append-only — the only write paths are INSERT (AuditRepo::append /
--     append_in_tx) and the retention DELETE (AuditRepo::sweep_before). There is
--     NO UPDATE and NO `FOR UPDATE` anywhere on the table, so the subtle
--     partition × concurrent-row-update interactions do not apply.
--   * It DOES carry two OUTBOUND FKs (it is the *referencing* side):
--       - workspace_id REFERENCES workspaces(id) ON DELETE CASCADE
--       - actor_id     REFERENCES participants(id)
--     PostgreSQL (this deploy is pg17) fully supports a partitioned table being
--     the referencing side of an FK: the constraint is declared once on the parent
--     and is inherited by every partition, and the ON DELETE CASCADE from the
--     workspaces side reaches rows in every partition. We re-declare both FKs on
--     the new parent below so the workspace-delete cascade keeps working exactly
--     as before (covered by workspace/workspace_impl.rs's cascade test).
--
-- webhook_delivery_log  → SKIPPED (NOT partitioned — see report).
--   It has 0 inbound FKs too, but it is a mutable retry/DLQ STATE MACHINE, not an
--   append-only log: mark_delivered / mark_failed_with_backoff / requeue UPDATE
--   rows in place, and claim_due uses `FOR UPDATE SKIP LOCKED` to claim due rows.
--   That is exactly the "frequently UPDATE / FOR UPDATE'd" hot path the rollout
--   guidance says to skip, so it is deliberately left as a plain heap (its
--   existing status-scoped retention DELETE in sweep_terminal_before bounds it).
--
-- ==========================================================================
-- PARTITION KEY: created_at (RANGE), one partition per UTC day.
--   * created_at is the retention column (sweep_before deletes WHERE created_at <
--     cutoff), so daily partitions turn retention into a cheap DROP PARTITION.
--   * Every UNIQUE/PK on a partitioned table must contain the partition key, so the
--     PK becomes (id, created_at) instead of (id). `id` is a ULID-in-UUID surrogate
--     that is globally unique anyway and no query filters by id alone — the
--     tenant listing keyset walks (workspace_id, id DESC) — so (id, created_at) is a
--     strictly looser constraint with no behavioural change.
--
-- SAFE SEQUENCE (single transaction — sqlx wraps each migration file in one):
--   1. RENAME the existing table + its indexes out of the way.
--   2. CREATE the new partitioned parent with the same columns + (id, created_at) PK
--      and the same two outbound FKs.
--   3. Create a DEFAULT catch-all partition + today's and tomorrow's daily ones, so
--      an INSERT always has a home even if partition maintenance lags.
--   4. Copy every legacy row into the parent (routed to the right partition).
--   5. DROP the legacy table.
--   6. Recreate the (workspace_id, id DESC) tenant-listing index on the parent.
--
-- IDEMPOTENT-ISH: a one-way structural change, guarded by a DO block so re-running
--   it on an already-partitioned table is a clean no-op.

DO $migrate$
BEGIN
    -- Only convert when audit_events is still a plain (non-partitioned) table.
    -- If it is already partitioned (this migration already applied), skip the
    -- whole block so a re-run is a no-op.
    IF EXISTS (
        SELECT 1
          FROM pg_class c
          JOIN pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relname = 'audit_events'
           AND n.nspname = 'public'
           AND c.relkind = 'r'            -- ordinary table, NOT 'p' (partitioned)
    ) THEN

        -- 1) Move the current table + indexes aside.
        ALTER TABLE audit_events RENAME TO audit_events_legacy;
        ALTER INDEX audit_events_pkey
            RENAME TO audit_events_legacy_pkey;
        ALTER INDEX audit_events_workspace_idx
            RENAME TO audit_events_legacy_workspace_idx;

        -- 2) New partitioned parent. Same columns/defaults as 0007; the PK gains
        --    created_at to satisfy the partition-key rule. The two outbound FKs are
        --    re-declared on the parent and are inherited by every partition (pg17),
        --    so the ON DELETE CASCADE from workspaces still reaches audit rows.
        CREATE TABLE audit_events (
            id           UUID        NOT NULL,
            workspace_id UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
            actor_id     UUID        REFERENCES participants(id),
            action       TEXT        NOT NULL,
            target       TEXT,
            detail       JSONB       NOT NULL DEFAULT '{}'::jsonb,
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (id, created_at)
        ) PARTITION BY RANGE (created_at);

        -- 3a) DEFAULT catch-all partition: an INSERT whose created_at has no daily
        --     partition still succeeds (never a "no partition" error if maintenance
        --     lags).
        CREATE TABLE audit_events_default
            PARTITION OF audit_events DEFAULT;

        -- 3b) Today's and tomorrow's daily partitions (UTC). Future ones are rolled
        --     by ensure_audit_event_partitions() (called from the retention sweep).
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF audit_events '
            || 'FOR VALUES FROM (%L) TO (%L)',
            'audit_events_' || to_char((now() AT TIME ZONE 'UTC')::date, 'YYYYMMDD'),
            (now() AT TIME ZONE 'UTC')::date,
            ((now() AT TIME ZONE 'UTC')::date + 1)
        );
        EXECUTE format(
            'CREATE TABLE %I PARTITION OF audit_events '
            || 'FOR VALUES FROM (%L) TO (%L)',
            'audit_events_' || to_char(((now() AT TIME ZONE 'UTC')::date + 1), 'YYYYMMDD'),
            ((now() AT TIME ZONE 'UTC')::date + 1),
            ((now() AT TIME ZONE 'UTC')::date + 2)
        );

        -- 4) Copy legacy rows into the parent (router sends each to its partition;
        --    any row whose day has no daily partition lands in DEFAULT).
        INSERT INTO audit_events (id, workspace_id, actor_id, action, target, detail, created_at)
            SELECT id, workspace_id, actor_id, action, target, detail, created_at
              FROM audit_events_legacy;

        -- 5) Drop the legacy table.
        DROP TABLE audit_events_legacy;

        -- 6) Recreate the tenant-listing index on the parent (cascades to all
        --    partitions). The id is a time-sortable ULID so this serves both the
        --    workspace filter and the reverse-chronological keyset cursor.
        CREATE INDEX audit_events_workspace_idx
            ON audit_events (workspace_id, id DESC);

    END IF;
END
$migrate$;

-- ---------------------------------------------------------------------------
-- PARTITION MAINTENANCE
--
-- ensure_audit_event_partitions() does both halves of daily-partition upkeep in
-- one idempotent call: (a) pre-create [today-1 .. today+ahead] daily partitions so
-- inserts always have a home, and (b) DROP daily partitions strictly older than
-- `keep_days` (the audit-retention window) — a metadata-only reclaim that replaces
-- the row-by-row DELETE sweep. The retention loop (boot/retention.rs) invokes it
-- each cycle so no external cron is required. The DEFAULT partition is never
-- created/dropped here, so an insert never fails between maintenance runs, and a
-- partition that could still hold rows inside the retention window is never
-- dropped — so it is safe to over-call.
CREATE OR REPLACE FUNCTION ensure_audit_event_partitions(
    keep_days INT DEFAULT 365,  -- drop daily partitions older than this (audit window)
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
        part_name := 'audit_events_' || to_char(d, 'YYYYMMDD');
        IF NOT EXISTS (
            SELECT 1 FROM pg_class WHERE relname = part_name
        ) THEN
            EXECUTE format(
                'CREATE TABLE %I PARTITION OF audit_events '
                || 'FOR VALUES FROM (%L) TO (%L)',
                part_name, d, d + 1
            );
        END IF;
    END LOOP;

    -- (b) Drop daily partitions entirely older than the retention window. Only
    --     partitions named audit_events_YYYYMMDD are considered; the DEFAULT
    --     partition (no date suffix) is never matched, so it survives.
    FOR r IN
        SELECT c.relname
          FROM pg_inherits i
          JOIN pg_class c     ON c.oid = i.inhrelid
          JOIN pg_class p     ON p.oid = i.inhparent
         WHERE p.relname = 'audit_events'
           AND c.relname ~ '^audit_events_[0-9]{8}$'
    LOOP
        IF to_date(right(r.relname, 8), 'YYYYMMDD') < drop_before THEN
            EXECUTE format('DROP TABLE IF EXISTS %I', r.relname);
        END IF;
    END LOOP;
END
$fn$;
