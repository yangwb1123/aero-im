-- Snaplink Audit Governance delivery and rolling activation recovery.

CREATE OR REPLACE FUNCTION aero_sanitize_snaplink_audit_value(value JSONB)
RETURNS JSONB
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    sanitized JSONB;
BEGIN
    IF jsonb_typeof(value) = 'object' THEN
        SELECT COALESCE(
                   jsonb_object_agg(entry.key,
                       aero_sanitize_snaplink_audit_value(entry.value)),
                   '{}'::jsonb
               )
          INTO sanitized
          FROM jsonb_each(value) entry
         WHERE lower(replace(entry.key, '-', '_')) NOT LIKE '%password%'
           AND lower(replace(entry.key, '-', '_')) NOT LIKE '%token%'
           AND lower(replace(entry.key, '-', '_')) NOT LIKE '%private_key%'
           AND lower(replace(entry.key, '-', '_')) NOT LIKE '%device_credential%'
           AND lower(replace(entry.key, '-', '_')) NOT LIKE '%card_number%'
           AND lower(replace(entry.key, '-', '_')) NOT LIKE '%secret%';
        RETURN sanitized;
    END IF;
    IF jsonb_typeof(value) = 'array' THEN
        SELECT COALESCE(
                   jsonb_agg(aero_sanitize_snaplink_audit_value(entry.value)
                             ORDER BY entry.ordinality),
                   '[]'::jsonb
               )
          INTO sanitized
          FROM jsonb_array_elements(value) WITH ORDINALITY AS entry(value, ordinality);
        RETURN sanitized;
    END IF;
    RETURN value;
END
$$;

CREATE OR REPLACE FUNCTION aero_snaplink_audit_payload(detail JSONB)
RETURNS JSONB
LANGUAGE plpgsql
IMMUTABLE
AS $$
DECLARE
    projected JSONB;
    projected_bytes INTEGER;
BEGIN
    projected := CASE
        WHEN jsonb_typeof(detail) = 'object' THEN detail
        ELSE jsonb_build_object('value', detail)
    END;
    projected := aero_sanitize_snaplink_audit_value(projected);
    projected_bytes := octet_length(projected::text);
    IF projected_bytes > 65536 THEN
        RETURN jsonb_build_object(
            'omitted', TRUE,
            'reason', 'payload_size_limit',
            'sha256', encode(digest(projected::text, 'sha256'), 'hex'),
            'original_bytes', projected_bytes
        );
    END IF;
    RETURN projected;
END
$$;

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
        binding.client_id,
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

DROP TRIGGER IF EXISTS audit_events_snaplink_delivery ON audit_events;
CREATE TRIGGER audit_events_snaplink_delivery
    AFTER INSERT ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_snaplink_audit();

-- New rows use the atomic triggers. These bounded functions recover rows
-- accepted before the database-wide commercial switch was enabled. Delivered
-- rows remain as the durable reconciliation cursor.
CREATE OR REPLACE FUNCTION aero_reconcile_snaplink_usage(max_rows INTEGER)
RETURNS INTEGER
LANGUAGE plpgsql
AS $$
DECLARE
    item RECORD;
    usage_dimension TEXT;
    fact_id TEXT;
    inserted INTEGER;
    total INTEGER := 0;
BEGIN
    FOR item IN
        SELECT message.id, message.sender_id, message.metadata, message.created_at,
               room.workspace_id, binding.tenant_id, binding.client_id,
               binding.source_system,
               EXISTS (
                   SELECT 1
                     FROM integration_installations installation
                    WHERE message.metadata ->> 'source' = 'integration'
                      AND installation.id::text =
                          message.metadata ->> 'installation_id'
                      AND installation.workspace_id = room.workspace_id
                      AND installation.bot_id = message.sender_id
               ) AS trusted_integration
          FROM messages message
          JOIN rooms room ON room.id = message.room_id
          JOIN snaplink_commercial_bindings binding
            ON binding.workspace_id = room.workspace_id AND binding.enabled
         WHERE message.created_at >=
               (date_trunc('month', clock_timestamp() AT TIME ZONE 'UTC')
                AT TIME ZONE 'UTC')
           AND NOT EXISTS (
                 SELECT 1 FROM snaplink_delivery_outbox outbox
                  WHERE outbox.delivery_id IN (
                        'usage:aero-im:messages_per_month:' || message.id::text,
                        'usage:aero-im:notifications_per_month:' || message.id::text
                  )
           )
         ORDER BY message.created_at, message.id
         LIMIT LEAST(GREATEST(max_rows, 1), 500)
    LOOP
        usage_dimension := CASE
            WHEN item.trusted_integration
                THEN 'notifications_per_month'
            ELSE 'messages_per_month'
        END;
        fact_id := 'aero-im:' || usage_dimension || ':' || item.id::text;
        INSERT INTO snaplink_delivery_outbox
               (delivery_id, destination, workspace_id, tenant_id, client_id,
                source_system, idempotency_key, payload, occurred_at)
        VALUES (
            'usage:' || fact_id,
            'usage',
            item.workspace_id,
            item.tenant_id,
            item.client_id,
            item.source_system,
            fact_id,
            jsonb_build_object(
                'id', fact_id,
                'dimension', usage_dimension,
                'quantity', 1,
                'occurred_at', item.created_at,
                'metadata', jsonb_build_object(
                    'workspace_id', item.workspace_id::text,
                    'message_id', item.id::text,
                    'product', 'aero-im'
                )
            ),
            item.created_at
        )
        ON CONFLICT (delivery_id) DO NOTHING;
        GET DIAGNOSTICS inserted = ROW_COUNT;
        IF inserted = 1 THEN
            INSERT INTO snaplink_usage_counters
                   (workspace_id, dimension, period_start, committed)
            VALUES (
                item.workspace_id,
                usage_dimension,
                date_trunc('month', item.created_at AT TIME ZONE 'UTC')::date,
                1
            )
            ON CONFLICT (workspace_id, dimension, period_start) DO UPDATE
                SET committed = snaplink_usage_counters.committed + 1,
                    updated_at = clock_timestamp();
            total := total + 1;
        END IF;
    END LOOP;
    RETURN total;
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
               binding.client_id, binding.source_system
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
            item.client_id,
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
