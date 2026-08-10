-- Migration 0242: L1 window aggregation for the message.*/room.* backlog lanes (B5-1 core).
--
-- The relay stops being moderation-only: `message.create`/`message.edit`
-- audit rows aggregate into ONE outbox row per (workspace, class, 60s
-- window) instead of flowing 1:1 — O(windows) not O(N). Admin-class
-- moderation rows (message.moderated → 0239) stay 1:1, untouched.
--
-- Cross-slice pins (textual — aero-storage must never import aero-ai, so the
-- literal + the behavioral db_tests are the drift guards):
--   * allowlist tokens 'message.create'/'message.edit' =
--     LOCAL_ACTION_MESSAGE_CREATE / LOCAL_ACTION_MESSAGE_EDIT
--     (crates/aero-common/src/model/audit.rs).
--   * class 'message' = GOVERNANCE_CLASS_MESSAGE; priority 10 =
--     GOVERNANCE_PRIORITY_BACKLOG (aero_ai::governance.rs:33).
--   * envelope action 'message.batch' = AGGREGATED_MESSAGE_ACTION; source
--     'aero-im.source' = AUDIT_SOURCE_SYSTEM (deployment invariant:
--     AERO_AUDIT_SOURCE_SYSTEM must equal this value — the connector's
--     validate_delivery_payload rejects any payload whose source_system
--     differs, PayloadGuard permanent ⇒ dead after ≤1 retry).
--   * window divisor 60 = L1_WINDOW_SECONDS.
--   * The aero-storage db_tests `l1_window_aggregates_*` recompute the
--     window key and assert the envelope field-by-field from those constants
--     (never inline literals) — a drift in any literal above breaks the
--     equality (G2 closure: truth-check scans crates --glob '*.rs' only, so
--     this SQL side is unpoliced; the db_test IS the pin).
--
-- Merge contract (reviewer-mandated, all three adversarial reviews):
--   * Merge ONLY into `status = 0` rows. `claim_due` (pg.rs) snapshots the
--     payload at claim (`RETURNING outbox.payload`) and `settle` never
--     re-reads it; merging into a claimed (status=1) row would rewrite the
--     in-flight snapshot the sink already holds → silent sink undercount on
--     the delivery-success path. `IN (0,1)` is a regression, not a widening.
--   * `ROW_COUNT = 0` ⇔ conflict-with-WHERE-false is exact (no DELETE path
--     on the table in production — all DELETEs are test cleanup), so a
--     status 1/2/3 window row spills uniformly. The spill row carries its
--     OWN deterministic key, OWN idempotency_key and OWN payload event_id
--     (the stub receipt echo reads payload.event_id; a window-key echo on a
--     spill row would ReceiptMismatch-dead it after ≤1 retry — sibling F1).
--   * Spill key `md5(v_key || '|' || NEW.id::text)::uuid` is deterministic
--     per (window, event): a replayed audit INSERT recomputes the same key
--     → ON CONFLICT DO NOTHING → at-most-one spill row per event.
--   * The merge/set re-evaluation on a concurrently-committed window row is
--     a READ COMMITTED EvalPlanQual behavior; the guarantee is READ
--     COMMITTED-only (the pool sets only statement_timeout = '10000', no
--     isolation override). REPEATABLE READ/SERIALIZABLE would turn
--     same-window merges into 40001 (message-tx abort). Pinned by the
--     concurrent-merge db_test.
--   * Fail-closed abort blast radius: an error in this body aborts the
--     audit INSERT and the whole message tx (AFTER-trigger semantics; the
--     0239 insert in the same statement rolls back with it — no partial
--     state). The body is sub-query-free; the only cast
--     `(payload->>'count')::int` is safe under the sole-writer invariant
--     (the only writers of message-class rows are this trigger). Unlike
--     0239's Gate 2, there is deliberately NO runtime gate and NO binding
--     lookup (D2: no disabled-window gap, no message-lane reconciler). The
--     ops off-switch, if ever needed, is a DROP TRIGGER hotfix migration.
--
-- Firing order (same-event AFTER INSERT triggers run by TRIGGER NAME):
-- audit_events_governance_enqueue < audit_events_l1_aggregate <
-- audit_events_snaplink_delivery. This trigger only inserts into
-- audit_governance_outbox rows the 0239 trigger never touches (token
-- disjoint), so order is inert.

CREATE OR REPLACE FUNCTION aero_enqueue_l1_aggregate_audit()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
DECLARE
    v_key          TEXT;         -- workspace|class|window (key preimage)
    v_window_epoch BIGINT;       -- floor(epoch(created_at)/60)
    v_window_start TIMESTAMPTZ;
    v_window_end   TIMESTAMPTZ;
    v_event_id     UUID;         -- window row PK = md5(v_key)::uuid
    v_spill_key    UUID;         -- spill row PK = md5(v_key|'|'|NEW.id)::uuid
    v_merged       INTEGER;
BEGIN
    -- Allowlist gate (fail-open pass-through): ONLY the L1-aggregatable
    -- message-lane tokens enter the window. message.moderated (admin 1:1,
    -- is_admin_class), room.* and unknown rows pass through untouched —
    -- never raise, never block (mirrors the 0239 pass-through contract).
    IF NEW.action NOT IN ('message.create', 'message.edit') THEN
        RETURN NEW;
    END IF;

    -- Window key: deterministic + replay-stable. created_at is
    -- server-stamped (AuditRepo::append_on), single clock domain.
    v_window_epoch := floor(extract(epoch FROM NEW.created_at) / 60)::bigint;
    v_key := NEW.workspace_id::text || '|' || 'message' || '|' || v_window_epoch::text;
    v_event_id := md5(v_key)::uuid;
    v_window_start := to_timestamp(v_window_epoch * 60);
    v_window_end := v_window_start + make_interval(secs => 60); -- L1_WINDOW_SECONDS

    -- Merge (primary path): status = 0 ONLY (see header contract). The
    -- first row's created_at becomes first_event_at; every merge advances
    -- last_event_at and increments count (sole writer ⇒ cast safe).
    INSERT INTO audit_governance_outbox
           (event_id, status, class, priority, payload)
    VALUES (
        v_event_id,
        0,
        'message',   -- GOVERNANCE_CLASS_MESSAGE
        10,          -- GOVERNANCE_PRIORITY_BACKLOG (0239 DEFAULT)
        jsonb_build_object(
            'event_id', v_event_id::text,             -- own PK (receipt echo source)
            'source_system', 'aero-im.source',        -- AUDIT_SOURCE_SYSTEM (payload guard)
            'event_type', 'aero.im.security',         -- AUDIT_EVENT_TYPE
            'schema_id', 'aero.im.security',          -- AUDIT_SCHEMA_ID
            'schema_version', 1,                      -- AUDIT_SCHEMA_VERSION
            'occurred_at', v_window_start,
            'aggregate_type', 'workspace',            -- AUDIT_AGGREGATE_TYPE
            'aggregate_id', NEW.workspace_id::text,
            'action', 'message.batch',                -- AGGREGATED_MESSAGE_ACTION
            'outcome', 'success',                     -- AUDIT_OUTCOME_SUCCESS
            'data_classification', 'confidential',    -- AUDIT_DATA_CLASSIFICATION
            'retention_class', 'security',            -- AUDIT_RETENTION_CLASS
            'idempotency_key', v_event_id::text,      -- own key (sink dedup)
            'count', 1,
            'aggregated', true,                       -- parity-exemption key (top-level)
            'window_start', v_window_start,
            'window_end', v_window_end,
            'first_event_at', NEW.created_at,
            'last_event_at', NEW.created_at
        )
    )
    ON CONFLICT (event_id) DO UPDATE
        SET payload = audit_governance_outbox.payload || jsonb_build_object(
                'count', (audit_governance_outbox.payload->>'count')::int + 1,
                'last_event_at', NEW.created_at
            )
        WHERE audit_governance_outbox.status = 0;
    GET DIAGNOSTICS v_merged = ROW_COUNT;

    -- Spill path: ROW_COUNT = 0 ⇔ the window row exists with status 1/2/3
    -- (or, impossibly under sole-writer, a foreign conflict). The late event
    -- is never dropped and never rewrites the frozen in-flight snapshot.
    IF v_merged = 0 THEN
        v_spill_key := md5(v_key || '|' || NEW.id::text)::uuid;
        INSERT INTO audit_governance_outbox
               (event_id, status, class, priority, payload)
        VALUES (
            v_spill_key,
            0,
            'message',   -- GOVERNANCE_CLASS_MESSAGE
            10,          -- GOVERNANCE_PRIORITY_BACKLOG
            jsonb_build_object(
                'event_id', v_spill_key::text,        -- own PK (receipt echo source)
                'source_system', 'aero-im.source',    -- AUDIT_SOURCE_SYSTEM
                'event_type', 'aero.im.security',
                'schema_id', 'aero.im.security',
                'schema_version', 1,
                'occurred_at', NEW.created_at,
                'aggregate_type', 'workspace',
                'aggregate_id', NEW.workspace_id::text,
                'action', 'message.batch',            -- AGGREGATED_MESSAGE_ACTION
                'outcome', 'success',
                'data_classification', 'confidential',
                'retention_class', 'security',
                'idempotency_key', v_spill_key::text, -- own key — never the window key
                'count', 1,
                'aggregated', true,                   -- parity-exemption key
                'spill', true,                        -- shape discriminator (parity SUM side)
                'window_start', v_window_start,
                'window_end', v_window_end,
                'first_event_at', NEW.created_at,
                'last_event_at', NEW.created_at
            )
        )
        ON CONFLICT (event_id) DO NOTHING; -- replay-idempotent per (window, event)
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS audit_events_l1_aggregate ON audit_events;
CREATE TRIGGER audit_events_l1_aggregate
    AFTER INSERT ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_l1_aggregate_audit();
