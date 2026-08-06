-- Split Billing and Audit Governance service identities at the durable binding.
--
-- Existing installations temporarily copy the former shared client into the
-- audit column so the migration is data preserving. The version-2 desired-state
-- file must then advance each existing binding revision and install a distinct
-- audit client before the process can accept traffic.

ALTER TABLE snaplink_commercial_bindings
    ADD COLUMN IF NOT EXISTS audit_client_id TEXT;

-- The v1 update guard treats every UPDATE as a desired-state transition and
-- requires revision +1. This data-preserving backfill is a schema transition,
-- so remove the old trigger before touching existing rows. The destination-
-- aware replacement is installed below in the same migration transaction.
DROP TRIGGER IF EXISTS snaplink_commercial_binding_update_guard
    ON snaplink_commercial_bindings;

UPDATE snaplink_commercial_bindings
   SET audit_client_id = client_id
 WHERE audit_client_id IS NULL;

ALTER TABLE snaplink_commercial_bindings
    ALTER COLUMN audit_client_id SET NOT NULL;

ALTER TABLE snaplink_commercial_bindings
    ADD CONSTRAINT snaplink_commercial_audit_client_valid CHECK (
        octet_length(audit_client_id) BETWEEN 1 AND 512
        AND audit_client_id = btrim(audit_client_id)
        AND audit_client_id !~ '[[:cntrl:]]'
    );

ALTER TABLE snaplink_commercial_bindings
    ADD CONSTRAINT snaplink_commercial_audit_client_unique UNIQUE (audit_client_id);

CREATE OR REPLACE FUNCTION aero_guard_snaplink_binding_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.client_id = NEW.audit_client_id THEN
        RAISE EXCEPTION 'Snaplink Billing and Audit clients must be distinct'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'snaplink_binding_destination_separation';
    END IF;
    IF EXISTS (
        SELECT 1
          FROM snaplink_commercial_bindings candidate
         WHERE candidate.workspace_id <> NEW.workspace_id
           AND (candidate.client_id IN (NEW.client_id, NEW.audit_client_id)
                OR candidate.audit_client_id IN (NEW.client_id, NEW.audit_client_id))
    ) THEN
        RAISE EXCEPTION 'Snaplink clients must be globally unique across destinations'
            USING ERRCODE = '23505',
                  CONSTRAINT = 'snaplink_binding_client_cross_role_unique';
    END IF;
    IF TG_OP = 'INSERT' THEN
        RETURN NEW;
    END IF;
    IF NEW.workspace_id <> OLD.workspace_id
       OR NEW.tenant_id <> OLD.tenant_id
       OR NEW.source_system <> OLD.source_system THEN
        RAISE EXCEPTION 'Snaplink binding identity is immutable'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'snaplink_binding_identity_immutable';
    END IF;
    IF NEW.revision <> OLD.revision + 1 THEN
        RAISE EXCEPTION 'Snaplink binding revision must advance by one'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'snaplink_binding_revision_sequence';
    END IF;
    IF NEW.client_id <> OLD.client_id AND EXISTS (
        SELECT 1 FROM snaplink_delivery_outbox outbox
         WHERE outbox.workspace_id = OLD.workspace_id
           AND outbox.destination = 'usage'
           AND outbox.client_id = OLD.client_id
           AND outbox.delivered_at IS NULL
    ) THEN
        RAISE EXCEPTION 'Snaplink Billing client cannot rotate with pending usage'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'snaplink_binding_billing_rotation_pending';
    END IF;
    IF NEW.audit_client_id <> OLD.audit_client_id AND EXISTS (
        SELECT 1 FROM snaplink_delivery_outbox outbox
         WHERE outbox.workspace_id = OLD.workspace_id
           AND outbox.destination = 'audit'
           AND outbox.client_id = OLD.audit_client_id
           AND outbox.delivered_at IS NULL
    ) THEN
        RAISE EXCEPTION 'Snaplink Audit client cannot rotate with pending audit events'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'snaplink_binding_audit_rotation_pending';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS snaplink_commercial_binding_update_guard
    ON snaplink_commercial_bindings;
CREATE TRIGGER snaplink_commercial_binding_update_guard
    BEFORE INSERT OR UPDATE ON snaplink_commercial_bindings
    FOR EACH ROW
    EXECUTE FUNCTION aero_guard_snaplink_binding_update();

CREATE OR REPLACE FUNCTION aero_enqueue_snaplink_audit()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    enforcement_enabled BOOLEAN;
    binding snaplink_commercial_bindings%ROWTYPE;
    target_list JSONB;
    event_payload JSONB;
BEGIN
    SELECT runtime.enabled INTO enforcement_enabled
      FROM snaplink_commercial_runtime runtime
     WHERE runtime.singleton;
    IF NOT COALESCE(enforcement_enabled, FALSE) THEN
        RETURN NEW;
    END IF;
    binding := aero_snaplink_binding_for_workspace(NEW.workspace_id);
    target_list := CASE
        WHEN NEW.target IS NULL THEN '[]'::jsonb
        ELSE jsonb_build_array(jsonb_build_object('id', NEW.target, 'type', 'resource'))
    END;
    event_payload := aero_snaplink_audit_payload(NEW.detail);

    INSERT INTO snaplink_delivery_outbox
           (delivery_id, destination, workspace_id, tenant_id, client_id,
            source_system, idempotency_key, payload, occurred_at)
    VALUES (
        'audit:' || NEW.id::text,
        'audit',
        NEW.workspace_id,
        binding.tenant_id,
        binding.audit_client_id,
        binding.source_system,
        NEW.id::text,
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
            'targets', target_list,
            'aggregate_type', 'workspace',
            'aggregate_id', NEW.workspace_id::text,
            'action', NEW.action,
            'outcome', 'success',
            'payload', event_payload,
            'data_classification', 'confidential',
            'retention_class', 'security',
            'idempotency_key', NEW.id::text
        ),
        NEW.created_at
    );
    RETURN NEW;
END
$$;

CREATE OR REPLACE FUNCTION aero_reconcile_snaplink_audit(max_rows INTEGER)
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
               binding.audit_client_id, binding.source_system
          FROM audit_events audit
          JOIN snaplink_commercial_bindings binding
            ON binding.workspace_id = audit.workspace_id AND binding.enabled
         WHERE NOT EXISTS (
               SELECT 1 FROM snaplink_delivery_outbox outbox
                WHERE outbox.delivery_id = 'audit:' || audit.id::text
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
        INSERT INTO snaplink_delivery_outbox
               (delivery_id, destination, workspace_id, tenant_id, client_id,
                source_system, idempotency_key, payload, occurred_at)
        VALUES (
            'audit:' || item.id::text,
            'audit',
            item.workspace_id,
            item.tenant_id,
            item.audit_client_id,
            item.source_system,
            item.id::text,
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
                'action', item.action,
                'outcome', 'success',
                'payload', event_payload,
                'data_classification', 'confidential',
                'retention_class', 'security',
                'idempotency_key', item.id::text
            ),
            item.created_at
        )
        ON CONFLICT (delivery_id) DO NOTHING;
        GET DIAGNOSTICS inserted = ROW_COUNT;
        total := total + inserted;
    END LOOP;
    RETURN total;
END
$$;
