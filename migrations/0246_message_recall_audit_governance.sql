-- Migration 0246: message-recall lane enqueue for the audit governance
-- outbox (B5-1 outbox enqueue coverage completion).
--
-- Closes the last in-scope message-lane gap: `message.recalled` (the recall
-- producer at crates/aero-storage/src/message/authorization.rs, in-tx via
-- AuditRepo::append_in_tx, migration 0238) is the only non-moderation
-- message.* token with no lane — it passed through 0239/0242/0245 fail-open
-- and stayed v1-only. This migration adds the message-recall lane in-tx
-- producer: a token-keyed AFTER INSERT trigger on audit_events with an
-- EXACT-token allowlist ('message.recalled' — NOT a 'message'-prefix
-- widening; a
-- prefix would fabricate governance claims for tokens no contract defined,
-- violating the 0241 header's "never fabricate" rule), writing 1:1 rows
-- (event_id = audit_events.id, PK = the pinned UNIQUE(event_id) dedup
-- contract) with class 'message', explicit priority 10
-- (= GOVERNANCE_PRIORITY_BACKLOG, the 0239 column default — spelled
-- explicitly, no reliance on the default), and the 0239-shaped 16-key
-- envelope with source_system = the pinned AUDIT_SOURCE_SYSTEM
-- 'aero-im.source' (deployment invariant: AERO_AUDIT_SOURCE_SYSTEM must
-- equal this value — the connector's validate_delivery_payload rejects any
-- payload whose source_system differs, PayloadGuard permanent => dead after
-- <=1 retry; same exposure 0242/0245 already accept).
--
-- No runtime gate, no binding lookup (0242/0245 D2 precedent, adopted
-- verbatim): recall rows enqueue even while snaplink_commercial_runtime.
-- enabled = FALSE and with no binding row. Consequences: (a) no disabled-
-- window gap => no reconciler extension needed (0241 stays moderation-only —
-- its scan is WHERE audit.action = 'message.moderated' + NOT EXISTS, recall
-- rows are never scanned/backfilled); (b) the moderation fail-closed Gate 2
-- RAISE (0239) is untouched (it lives in aero_enqueue_governance_audit, not
-- here — and its token check precedes Gate 2, so a recall INSERT can never
-- abort via 0239). The only pre-existing abort on the recall row is the v1
-- 0236 binding RAISE (live for every action) — 0246 introduces no new abort
-- class.
--
-- Firing order (same-event AFTER triggers run by TRIGGER NAME):
-- audit_events_governance_enqueue < audit_events_l1_aggregate <
-- audit_events_message_recall_enqueue < audit_events_room_enqueue <
-- audit_events_snaplink_delivery. Token sets are pairwise disjoint (0239:
-- message.moderated, 0242: message.create/message.edit, 0245:
-- room.create/room.archived, THIS migration: message.recalled), so order is
-- inert. BOTH wrong-token copy-paste directions are behaviorally netted by
-- the storage db_tests: this body written as `<> 'message.moderated'`
-- => recall_lane_outbox_parity gets 0 rows != 3; this body firing on
-- message.moderated => rust_produced_payload_matches_0239_envelope half A's
-- fetch_one errors on 2 rows.
--
-- WIDENING TRIPWIRE (documented, behavioral-only): the SQL allowlist has no
-- static scan pin (truth-check polices only the Rust literal family; the
-- SQL side is unpoliced by design, see the 0245 header). The pin layer is
-- the DB-backed suite: a widening to a 'message'-prefix pattern reds
--   * non_moderation_action_passes_through_unmapped (message.deleted
--     => 0 governance rows becomes 1),
--   * rust_produced_payload_matches_0239_envelope half A (fetch_one errors
--     on 2 rows for message.moderated),
--   * l1_window_aggregates_5_rows_to_1_outbox (count < 6 breaks).
-- message_lane_outbox_parity is widening-BLIND (its SUM side reads only
-- aggregated/spill rows; 1:1 rows never enter) — the scoped exact-count
-- asserts are the net.
--
-- FAIL-CLOSED abort blast radius: an error in this body aborts the audit
-- INSERT and the whole tx (AFTER-trigger semantics; the 0239/0242/0245
-- inserts in the same statement roll back with it — no partial state). The
-- body is sub-query-free and cast-free; the only non-builtin call is
-- aero_snaplink_audit_payload (0236, IMMUTABLE, already invoked by
-- 0236/0239/0241 for every audit row — no new abort source). All outbox
-- CHECKs admit the row: class 'message' (0239 CHECK), priority 10 > 0, status
-- 0, delivery_mode default 'push', payload is a jsonb object.
--
-- TRIGGER-ONLY OWNERSHIP declaration (0245 header, extended): the
-- trigger-owned token set is now message.moderated (0239) ·
-- message.create/message.edit (0242 L1 window) · room.create/room.archived
-- (0245) · message.recalled (THIS migration). For these tokens,
-- audit_governance_outbox rows are produced ONLY by SQL triggers (plus the
-- 0241 reconciler for message.moderated backfill). Rust NEVER writes the
-- outbox for them — the authorization.rs change for this migration is a
-- literal-spelling edit only (the token flows through
-- AuditRepo::append_in_tx, the single Rust-visible entry point, spelled via
-- aero_common::model::audit::LOCAL_ACTION_MESSAGE_RECALLED). message.deleted
-- is NOT trigger-owned (R-D2: stays unmapped — the sibling Rust outbox
-- write for it remains the planned path).
--
-- AUDIT-ROW IMMUTABILITY: audit_events is append-only — no UPDATE
-- audit_events exists anywhere (zero AFTER UPDATE/DELETE triggers; all five
-- triggers 0236/0239/0242/0245/THIS are AFTER INSERT). DELETE is
-- lifecycle-only: the legal-hold-guarded retention sweep
-- (AuditRepo::sweep_before, NOT EXISTS legal_holds) and the daily-partition
-- DROP in ensure_audit_event_partitions (keep_days-bounded), plus test/drill
-- isolation cleanup. The trail is never rewritten, and outbox rows are never
-- deleted while the relay runs.
--
-- L1-window interaction: none by construction — 0242's allowlist is
-- message.create/message.edit only, so message.recalled can never fold into
-- a message.batch window; the parity SUM side is untouched. The 1:1
-- requirement is load-bearing (recall counts must never corrupt create/edit
-- aggregates).
--
-- Rollback: DROP TRIGGER audit_events_message_recall_enqueue (hotfix
-- migration or manual; parent-trigger drop clones to the daily partitions).
-- Existing recall rows remain claimable by any relay version (claim_due is
-- class-agnostic, payload forwarded untyped) — never delete outbox rows
-- while the relay runs (v1 precedent). Function left defined is harmless
-- (0239/0242/0245 precedent).

CREATE OR REPLACE FUNCTION aero_enqueue_message_recall_audit()
RETURNS trigger
LANGUAGE plpgsql
SET search_path = pg_catalog, public
AS $$
BEGIN
    -- Exact-token allowlist (fail-open pass-through): ONLY the message.recalled
    -- token enters the outbox. Every other action returns before any work —
    -- never raise, never block (mirrors the 0239/0242/0245 pass-through
    -- contract). NOT a prefix widening: 'message.deleted'/'message.evil'
    -- pass through unmapped (v1 only) — fabricated claims are impossible; a
    -- future message token is a schema change (one leaf const + one SQL
    -- literal + one migration), never a silent widening.
    IF NEW.action <> 'message.recalled' THEN RETURN NEW;
    END IF;

    -- 1:1 row: event_id = NEW.id (audit_events.id), the pinned
    -- UNIQUE(event_id) dedup contract. audit_events' composite PK
    -- (id, created_at) admits a same-id duplicate at the DB level, so a
    -- replayed INSERT re-fires this trigger — ON CONFLICT DO NOTHING makes
    -- the replay at-most-once per event (0239/0245 precedent; note this is
    -- stronger than the v1 0236 lane, whose plain INSERT has no ON CONFLICT
    -- and dedups fail-loud on its own delivery_id PK).
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
        'message',       -- GOVERNANCE_CLASS_MESSAGE (0239 CHECK admits)
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

DROP TRIGGER IF EXISTS audit_events_message_recall_enqueue ON audit_events;
CREATE TRIGGER audit_events_message_recall_enqueue
    AFTER INSERT ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_message_recall_audit();
