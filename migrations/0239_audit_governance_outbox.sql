-- Migration 0239: audit governance outbox (B5-1).
--
-- v2 outbox for the audit relay: 1:1 with audit_events.id (event_id =
-- audit_events.id). Status machine 0=enqueued 1=claimed 2=delivered 3=dead
-- (pin: crates/aero-audit-connector/src/pg.rs STATUS_ENQUEUED..STATUS_DEAD).
-- Additive: the 0235/0236 v1 path (snaplink_delivery_outbox) is untouched.
--
-- Cross-slice pins (textual — aero-storage must never import aero-ai, so the
-- literal + the behavioral drills are the drift guards):
--   * DEFAULT 10 = aero_ai::governance::GOVERNANCE_PRIORITY_BACKLOG
--     (crates/aero-ai/src/governance.rs:33). B5-3's aero-bus design doc must
--     reconcile its "DEFAULT 0" to 10 at landing (handoff H1 in the 0239
--     design doc).
--   * DEFAULT 'message' = GOVERNANCE_CLASS_MESSAGE (governance.rs:34).
--   * The moderation stamp (class 'admin', priority 100, outbound action
--     'admin.content.flag') = GOVERNANCE_CLASS_ADMIN /
--     GOVERNANCE_PRIORITY_MODERATION / MODERATION_OUTBOUND_ACTION
--     (governance.rs:31/:33/:60), keyed on LOCAL_ACTION_MODERATED
--     'message.moderated' (:49).
--
-- Value CHECKs (db-reviewer hardening finding 4): class ∈ {admin,message,
-- room} (the governance.rs GOVERNANCE_CLASS_* lanes), priority > 0 (DESC
-- lane, higher = claimed first; a new lane value is a schema change, never a
-- silent typo), delivery_mode ∈ {push} (reserved lane; B5-3's delivery
-- policy extends the CHECK by migration). Pinned behaviorally by
-- `ddl_contract_defaults_and_checks` (aero-storage/src/audit_governance.rs).

CREATE TABLE IF NOT EXISTS audit_governance_outbox (
    event_id          UUID        PRIMARY KEY,        -- = audit_events.id (1:1, A2 join key; satisfies the pinned "UNIQUE(event_id)" dedup contract)
    status            INTEGER     NOT NULL DEFAULT 0
                      CHECK (status IN (0, 1, 2, 3)), -- connector status machine (pg.rs:27-30)
    class             TEXT        NOT NULL DEFAULT 'message'
                      CHECK (class IN ('admin', 'message', 'room')), -- 'admin'|'message'|'room' (governance.rs GOVERNANCE_CLASS_*); t11/relay drills omit it → default required; value CHECK (finding 4): typo'd class rejected
    priority          SMALLINT    NOT NULL DEFAULT 10
                      CHECK (priority > 0), -- DEFAULT 10 = GOVERNANCE_PRIORITY_BACKLOG (governance.rs:33); t11/relay drills omit it → default required; DESC lane: higher = claimed first (B5-3); value CHECK (finding 4): 0/negative rejected, new lane values land via migration
    delivery_mode     TEXT        NOT NULL DEFAULT 'push'
                      CHECK (delivery_mode IN ('push')), -- reserved (design-doc promised shape; no consumer yet — B5-3 delivery policy); value CHECK (finding 4): typo'd mode rejected, new modes land via migration
    payload           JSONB       NOT NULL
                      CHECK (jsonb_typeof(payload) = 'object'), -- relay forwards verbatim; sink Idempotency-Key = event_id
    available_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(), -- claim filter (pg.rs:82); single clock domain = clock_timestamp()
    attempts          BIGINT      NOT NULL DEFAULT 0 CHECK (attempts >= 0), -- post-increment per claim (pg.rs:97)
    claim_token       UUID,       -- rotated per claim (pg.rs:94)
    lease_expires_at  TIMESTAMPTZ,
    delivered_at      TIMESTAMPTZ, -- settle stamp (pg.rs:134)
    last_error        TEXT,       -- requeue/dead detail (pg.rs:137,150)
    created_at        TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(), -- claim ORDER BY tiebreak (pg.rs:87)
    CONSTRAINT audit_governance_claim_state CHECK (
        (claim_token IS NULL AND lease_expires_at IS NULL)
        OR (claim_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    ) -- mirror v1 0235:178-181
);

-- Due index: claim filter + ORDER BY (pg.rs:78-88) with status IN (0,1).
-- B5-3 extends ORDER BY with priority; the index change is B5-3's, not this slice's.
CREATE INDEX IF NOT EXISTS audit_governance_due_idx
    ON audit_governance_outbox (available_at, created_at, event_id)
    WHERE status IN (0, 1);

-- Token-keyed enqueue (R2): ONLY audit action 'message.moderated' enters the
-- governance outbox. Every other action passes through untouched (fail-open;
-- never raise, never block) — they keep flowing through the 0236 v1 trigger.
CREATE OR REPLACE FUNCTION aero_enqueue_governance_audit()  -- R2 name (corrections C5)
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    enforcement_enabled BOOLEAN;
    binding snaplink_commercial_bindings%ROWTYPE;
BEGIN
    -- Gate 1 (fail-open, runtime): mirror 0236:75-79. A2 half (4)
    -- moderation_finalize_runtime_disabled_commits_1_plus_0 pins |G|=0 when
    -- disabled — dropping this check breaks that half.
    SELECT runtime.enabled INTO enforcement_enabled
      FROM snaplink_commercial_runtime runtime
     WHERE runtime.singleton;
    IF NOT COALESCE(enforcement_enabled, FALSE) THEN
        RETURN NEW;
    END IF;

    -- Token-keyed mapping (fail-open pass-through): only the moderation token
    -- maps; unknown tokens must NOT raise (a second abort path would block all
    -- room.*/message.* audit rows through the shared trigger — design R-D2).
    IF NEW.action <> 'message.moderated' THEN
        RETURN NEW; -- cross-slice pin: governance_lane_for(other) == None
    END IF;

    -- Gate 2 (fail-closed, binding): mirror 0236:84. RAISE aborts the whole
    -- tx (soft delete + audit + outbox together). A2 half (3)
    -- moderation_finalize_without_binding_aborts_tx pins this; keeping the
    -- lookup in THIS trigger makes the abort independent of trigger order
    -- (audit_events_governance fires before audit_events_snaplink_delivery:
    -- 'g' < 's') and of the sibling's later redirect.
    binding := aero_snaplink_binding_for_workspace(NEW.workspace_id);

    -- cross-slice pin: must equal aero_ai::governance::governance_lane_for(
    --   "message.moderated") → GovernanceLane { class: "admin",
    --   priority: 100 /* GOVERNANCE_PRIORITY_MODERATION */,
    --   outbound_action: "admin.content.flag" /* MODERATION_OUTBOUND_ACTION */,
    --   status: 0 }
    INSERT INTO audit_governance_outbox
           (event_id, status, class, priority, payload)
    VALUES (
        NEW.id,
        0,
        'admin',
        100,
        jsonb_build_object(
            'event_id', NEW.id::text,
            'source_system', binding.source_system,
            'event_type', 'aero.im.security',
            'schema_id', 'aero.im.security',
            'schema_version', 1,
            'occurred_at', NEW.created_at,
            'actor', jsonb_build_object(
                'id', COALESCE(NEW.actor_id::text, 'system'),
                'type', CASE WHEN NEW.actor_id IS NULL THEN 'system' ELSE 'participant' END
            ),
            'targets', CASE WHEN NEW.target IS NULL THEN '[]'::jsonb
                            ELSE jsonb_build_array(jsonb_build_object(
                                'id', NEW.target, 'type', 'resource')) END,
            'aggregate_type', 'workspace',
            'aggregate_id', NEW.workspace_id::text,
            'action', 'admin.content.flag',  -- MODERATION_OUTBOUND_ACTION (A2 half 5 field assertion)
            'outcome', 'success',
            'payload', aero_snaplink_audit_payload(NEW.detail),
            'data_classification', 'confidential',
            'retention_class', 'security',
            'idempotency_key', NEW.id::text
        )
    )
    ON CONFLICT (event_id) DO NOTHING; -- idempotent vs sibling redirect / replay
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS audit_events_governance_enqueue ON audit_events;  -- R2 name (corrections C5)
CREATE TRIGGER audit_events_governance_enqueue
    AFTER INSERT ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_governance_audit();
