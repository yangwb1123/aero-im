-- Migration 0241: v2 disabled-window reconciler (B5-1 security Q1 closure).
--
-- Mirrors v1's `aero_reconcile_snaplink_audit` (0236:230) for the v2 lane:
-- the 0239 trigger only fires at INSERT time, so a `message.moderated` row
-- accepted while `snaplink_commercial_runtime.enabled = FALSE` commits its
-- audit row but never produces a v2 outbox row. The relay calls this
-- function before every claim batch (mirroring v1's dispatch loop, which
-- runs `reconcile_audit` ahead of `claim_due`), so the disabled window
-- self-heals on the first tick after re-enablement — no ops action, no
-- permanent gap, parity converges to COUNT(outbox) == COUNT(audit) for the
-- mapped subset.
--
-- v2-specific deviations from the v1 mirror (deliberate, each pinned):
--   * TOKEN-KEYED scan: v1 backfills every action (its lane is the general
--     billing/audit lane); the v2 lane is `message.moderated`-only because
--     the v2 envelope hardcodes action 'admin.content.flag' / class 'admin'
--     / priority 100. Backfilling an unmapped action would FABRICATE a
--     governance claim the trigger would never produce (governance.rs
--     R-D2: mis-tokened rows must stay out of the admin lane). The WHERE
--     filter mirrors the trigger's token-keyed mapping, not v1's
--     action-agnostic scan.
--   * event_id dedup: v1 keys on a constructed delivery_id string; v2 keys
--     on the `event_id` PRIMARY KEY (the same 1:1 key the trigger uses).
--   * INSERT is byte-identical to the 0239 trigger's INSERT (same literals:
--     status 0, class 'admin', priority 100, envelope with action
--     'admin.content.flag', actor/targets construction, sanitized payload)
--     so a backfilled row is indistinguishable from a live-enqueued row and
--     the A2 field assertions hold for both paths.
--   * NO runtime gate: v1's reconciler does not re-check
--     `snaplink_commercial_runtime.enabled` — the recovery path exists
--     precisely BECAUSE rows were missed while disabled. The operator
--     re-enables the switch, the next tick backfills. Re-applying the gate
--     here would make the function a no-op exactly when needed.
--
-- Parity-safe invariants (all enforced by NOT EXISTS + ON CONFLICT):
--   * idempotent: re-runs and concurrent runners return only newly inserted
--     rows; a row is inserted exactly once (unique event_id PK).
--   * dead rows are NEVER resurrected: status=3 rows exist in the outbox,
--     so NOT EXISTS skips them — the reconciler cannot fight the relay's
--     terminal state (403-outage recovery stays manual, per design).
--   * delivered rows are the durable reconciliation cursor (0236 comment
--     adopted verbatim): a delivered row is skipped, never re-delivered.
--   * retention-swept source rows are invisible to the scan (join side is
--     best-effort for rows older than the audit retention window — same
--     exposure as v1, prior-design §5(b)).
--   * enabled-binding join (identical to 0236): a row whose workspace has no
--     ENABLED binding cannot be addressed (no tenant_id/client_id/
--     source_system for the envelope) — it stays locally audited only,
--     which is the fail-open meaning of the gate.

CREATE OR REPLACE FUNCTION aero_reconcile_governance_audit(max_rows INTEGER)
RETURNS INTEGER
LANGUAGE plpgsql
AS $$
DECLARE
    item RECORD;
    inserted INTEGER;
    total INTEGER := 0;
    target_list JSONB;
    event_payload JSONB;
BEGIN
    FOR item IN
        SELECT audit.id, audit.workspace_id, audit.actor_id, audit.action,
               audit.target, audit.detail, audit.created_at, binding.tenant_id,
               binding.client_id, binding.source_system
          FROM audit_events audit
          JOIN snaplink_commercial_bindings binding
            ON binding.workspace_id = audit.workspace_id AND binding.enabled
         WHERE audit.action = 'message.moderated'  -- token-keyed (R-D2): only the moderation lane; never fabricate
           AND NOT EXISTS (
               SELECT 1 FROM audit_governance_outbox outbox
                WHERE outbox.event_id = audit.id
           )
         ORDER BY audit.created_at, audit.id
         LIMIT LEAST(GREATEST(max_rows, 1), 500)
    LOOP
        target_list := CASE
            WHEN item.target IS NULL THEN '[]'::jsonb
            ELSE jsonb_build_array(
                jsonb_build_object('id', item.target, 'type', 'resource')
            )
        END;
        event_payload := aero_snaplink_audit_payload(item.detail);
        -- Byte-identical to the 0239 trigger's INSERT (same literals; the
        -- A2 field assertions pin both paths). ON CONFLICT (event_id) DO
        -- NOTHING = the pinned UNIQUE(event_id) dedup contract; concurrent
        -- runners and re-runs converge instead of double-inserting.
        INSERT INTO audit_governance_outbox
               (event_id, status, class, priority, payload)
        VALUES (
            item.id,
            0,
            'admin',
            100,
            jsonb_build_object(
                'event_id', item.id::text,
                'source_system', item.source_system,
                'event_type', 'aero.im.security',
                'schema_id', 'aero.im.security',
                'schema_version', 1,
                'occurred_at', item.created_at,
                'actor', jsonb_build_object(
                    'id', COALESCE(item.actor_id::text, 'system'),
                    'type', CASE WHEN item.actor_id IS NULL
                                 THEN 'system' ELSE 'participant' END
                ),
                'targets', target_list,
                'aggregate_type', 'workspace',
                'aggregate_id', item.workspace_id::text,
                'action', 'admin.content.flag',  -- MODERATION_OUTBOUND_ACTION
                'outcome', 'success',
                'payload', event_payload,
                'data_classification', 'confidential',
                'retention_class', 'security',
                'idempotency_key', item.id::text
            )
        )
        ON CONFLICT (event_id) DO NOTHING;
        GET DIAGNOSTICS inserted = ROW_COUNT;
        total := total + inserted;
    END LOOP;
    RETURN total;
END
$$;
