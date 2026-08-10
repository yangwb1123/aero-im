-- Migration 0245: room-lane enqueue for the audit governance outbox (B5-1
-- completion).
--
-- RENUMBERED 0243 -> 0245 (adversarial N1 arbitration): 0243 =
-- `login_failures_created_at_idx` and 0244 = `audit_governance_failed_pairs`
-- (DLQ) are D8-finalized to the auth slice (docs/design/2026-08-08-aero-auth-
-- b5-1-in-tx-audit-outbox.design.md + docs/design/2026-08-08-aero-audit-
-- connector-b5-1-producer-side.design.md, both designed-only). 0245 is the
-- true next free number; the harness MIGRATION_COUNT arbiter flips to 243 in
-- the same commit (242 landed + this file; NOT 245 — the arbiter counts
-- actual files and 0243/0244 do not exist yet).
--
-- Closes the room-lane gap: class 'room' is CHECK-admitted by 0239
-- (CHECK (class IN ('admin','message','room'))) but nothing enqueues it.
-- This migration adds the room-lane in-tx producer: a token-keyed AFTER
-- INSERT trigger on audit_events with an EXACT-token allowlist
-- ('room.create'/'room.archived' — NOT a 'room.%' prefix; a prefix would
-- fabricate governance claims for tokens no contract defined, violating the
-- 0241 header's "never fabricate" rule), writing 1:1 rows
-- (event_id = audit_events.id, PK = the pinned UNIQUE(event_id) dedup
-- contract) with class 'room', explicit priority 10
-- (= GOVERNANCE_PRIORITY_BACKLOG, the 0239 column default — spelled
-- explicitly, no reliance on the default), and the 0239-shaped 16-key
-- envelope with source_system = the pinned AUDIT_SOURCE_SYSTEM
-- 'aero-im.source' (deployment invariant: AERO_AUDIT_SOURCE_SYSTEM must
-- equal this value — the connector's validate_delivery_payload rejects any
-- payload whose source_system differs, PayloadGuard permanent => dead after
-- <=1 retry; same exposure 0242 already accepts).
--
-- No runtime gate, no binding lookup (0242 D2 precedent, adopted verbatim):
-- room rows enqueue even while snaplink_commercial_runtime.enabled = FALSE
-- and with no binding row. Consequences: (a) no disabled-window gap => no
-- reconciler extension needed (0241 stays moderation-only — its scan is
-- WHERE audit.action = 'message.moderated' + NOT EXISTS, room rows are never
-- scanned/backfilled); (b) the moderation fail-closed Gate 2 RAISE (0239)
-- is untouched (it lives in aero_enqueue_governance_audit, not here).
--
-- Firing order (same-event AFTER triggers run by TRIGGER NAME):
-- audit_events_governance_enqueue < audit_events_l1_aggregate <
-- audit_events_room_enqueue < audit_events_snaplink_delivery. Token sets
-- are pairwise disjoint (0239: message.moderated, 0242:
-- message.create/message.edit, 0245: room.create/room.archived), so order
-- is inert.
--
-- FAIL-CLOSED abort blast radius: an error in this body aborts the audit
-- INSERT and the whole tx (AFTER-trigger semantics; the 0239/0242 inserts
-- in the same statement roll back with it — no partial state). The body is
-- sub-query-free and cast-free; the only non-builtin call is
-- aero_snaplink_audit_payload (0236, IMMUTABLE, already invoked by
-- 0236/0239/0241 for every audit row — no new abort source). All outbox
-- CHECKs admit the row: class 'room' (0239 CHECK), priority 10 > 0, status
-- 0, delivery_mode default 'push', payload is a jsonb object.
--
-- TRIGGER-ONLY OWNERSHIP declaration (adversarial N2 supersession, binding
-- on all six sibling AuditGovernanceOutboxRepo Rust-writer designs):
--   * Trigger-owned token set = message.moderated (0239) ·
--     message.create/message.edit (0242 L1 window) ·
--     room.create/room.archived (THIS migration). For these tokens,
--     audit_governance_outbox rows are produced ONLY by SQL triggers (plus
--     the 0241 reconciler for message.moderated backfill). Rust NEVER
--     writes the outbox for them.
--   * audit_events INSERT (AuditRepo::append_in_tx) is the single
--     Rust-visible entry point for room tokens: the deferred room-producer
--     slice appends audit rows with the leaf consts
--     (aero_common::model::audit::LOCAL_ACTION_ROOM_CREATE/ARCHIVED) and
--     MUST NOT write audit_governance_outbox — this trigger is the sole
--     outbox producer for these tokens.
--   * message.deleted is NOT trigger-owned (R-D2: stays unmapped — the
--     sibling Rust outbox write for it remains the planned path).
--   * Rule 3f (scripts/truth-check-lib.sh) enforces the literal side from
--     CI: no bare "room.create"/"room.archived" literal in production Rust
--     outside the leaf.
--
-- Rollback: DROP TRIGGER audit_events_room_enqueue (hotfix migration or
-- manual; parent-trigger drop clones to the daily partitions). Existing
-- room rows remain claimable by any relay version (claim_due is
-- class-agnostic, payload forwarded untyped) — never delete outbox rows
-- while the relay runs (v1 precedent). Function left defined is harmless
-- (0239/0242 precedent).

CREATE OR REPLACE FUNCTION aero_enqueue_room_audit()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    -- Exact-token allowlist (fail-open pass-through): ONLY the two room
    -- family tokens enter the outbox. Every other action returns before any
    -- work — never raise, never block (mirrors the 0239/0242 pass-through
    -- contract). NOT a LIKE 'room.%' prefix: 'room.creat'/'room.evil' pass
    -- through unmapped (v1 only) — fabricated claims are impossible; a
    -- future room token is a schema change (one leaf const + one SQL
    -- literal + one migration), never a silent widening.
    IF NEW.action NOT IN ('room.create', 'room.archived') THEN
        RETURN NEW;
    END IF;

    -- 1:1 row: event_id = NEW.id (audit_events.id), the pinned
    -- UNIQUE(event_id) dedup contract. audit_events' composite PK
    -- (id, created_at) admits a same-id duplicate at the DB level, so a
    -- replayed INSERT re-fires this trigger — ON CONFLICT DO NOTHING makes
    -- the replay at-most-once per event (0239 precedent).
    --
    -- Envelope: the 0239 16-key shape byte-identically (event_id,
    -- source_system, event_type, schema_id, schema_version, occurred_at,
    -- actor, targets, aggregate_type, aggregate_id, action, outcome,
    -- payload, data_classification, retention_class, idempotency_key) —
    -- parseable by the typed twin AuditClaimPayload (deny_unknown_fields,
    -- so any drift reds the parity db_test). No aggregated/spill/count/
    -- window keys: 1:1 rows never carry L1 markers, so the parity SUM side
    -- (0242 drill) never sees them.
    INSERT INTO audit_governance_outbox
           (event_id, status, class, priority, payload)
    VALUES (
        NEW.id,
        0,
        'room',          -- GOVERNANCE_CLASS_ROOM (0239 CHECK admits)
        10,              -- GOVERNANCE_PRIORITY_BACKLOG — explicit, not the column default
        jsonb_build_object(
            'event_id', NEW.id::text,             -- own PK (receipt echo source)
            'source_system', 'aero-im.source',    -- AUDIT_SOURCE_SYSTEM (payload guard)
            'event_type', 'aero.im.security',     -- AUDIT_EVENT_TYPE
            'schema_id', 'aero.im.security',      -- AUDIT_SCHEMA_ID
            'schema_version', 1,                  -- AUDIT_SCHEMA_VERSION
            'occurred_at', NEW.created_at,        -- server-stamped, single clock domain
            'actor', jsonb_build_object(
                'id', COALESCE(NEW.actor_id::text, 'system'),
                'type', CASE WHEN NEW.actor_id IS NULL THEN 'system' ELSE 'participant' END
            ),
            'targets', CASE WHEN NEW.target IS NULL THEN '[]'::jsonb
                            ELSE jsonb_build_array(jsonb_build_object(
                                'id', NEW.target, 'type', 'resource')) END,
            'aggregate_type', 'workspace',        -- AUDIT_AGGREGATE_TYPE
            'aggregate_id', NEW.workspace_id::text,
            'action', NEW.action,                 -- local token VERBATIM (no fabricated contract token)
            'outcome', 'success',                 -- AUDIT_OUTCOME_SUCCESS
            'payload', aero_snaplink_audit_payload(NEW.detail),
            'data_classification', 'confidential', -- AUDIT_DATA_CLASSIFICATION
            'retention_class', 'security',        -- AUDIT_RETENTION_CLASS
            'idempotency_key', NEW.id::text       -- = event_id (sink dedup)
        )
    )
    ON CONFLICT (event_id) DO NOTHING; -- replay-idempotent per audit event
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS audit_events_room_enqueue ON audit_events;
CREATE TRIGGER audit_events_room_enqueue
    AFTER INSERT ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_room_audit();
