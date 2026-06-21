-- 0154_audit_partition_legal_hold.sql — make audit retention legal-hold-aware
-- (ROADMAP 第六版 · 方向五·② · 审计不可变性).
--
-- The audit trail is aged out two ways: the row-level DELETE sweep
-- (`AuditRepo::sweep_before`) and the daily-partition DROP in
-- `ensure_audit_event_partitions` (0146). Neither consulted `legal_holds`, so a
-- workspace under an active legal hold could lose the audit events that are the
-- evidence for the very data the hold preserves — exactly the immutability gap
-- the message sweep already closes (`workspace/sweep.rs` `NOT EXISTS legal_holds`).
--
-- This redefines the partition-maintenance function so the DROP half NEVER drops
-- a daily partition that still contains audit events for a workspace with an
-- active hold. (The row-DELETE half gets the same `NOT EXISTS` guard in
-- `sweep_before`.) Once the hold is released, a later sweep cycle reclaims the
-- partition. Pure `CREATE OR REPLACE` — idempotent, no schema change.

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
    held         boolean;
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
            -- Legal-hold immutability (方向五): never drop a partition still
            -- holding events for a workspace under an active hold — the audit
            -- trail is evidence for the held data and must outlive retention while
            -- the hold stands. Released holds let the next cycle reclaim it.
            EXECUTE format(
                'SELECT EXISTS (SELECT 1 FROM %I ae '
                || 'JOIN legal_holds lh ON lh.active AND lh.workspace_id = ae.workspace_id)',
                r.relname
            ) INTO held;
            IF NOT held THEN
                EXECUTE format('DROP TABLE IF EXISTS %I', r.relname);
            END IF;
        END IF;
    END LOOP;
END
$fn$;
