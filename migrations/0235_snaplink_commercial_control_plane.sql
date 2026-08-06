-- Snaplink-backed commercial enforcement and governance delivery.
--
-- Message quota consumption and both outbound facts are created at the
-- PostgreSQL write boundary.  This keeps raw SQL writers inside the same
-- entitlement boundary and makes the business row, local counter, usage fact,
-- and security audit relay atomic.

CREATE TABLE IF NOT EXISTS snaplink_commercial_runtime (
    singleton  BOOLEAN     PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    enabled    BOOLEAN     NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

INSERT INTO snaplink_commercial_runtime (singleton, enabled)
VALUES (TRUE, FALSE)
ON CONFLICT (singleton) DO NOTHING;

CREATE TABLE IF NOT EXISTS snaplink_commercial_bindings (
    workspace_id  UUID        PRIMARY KEY,
    tenant_id     TEXT        NOT NULL UNIQUE,
    client_id     TEXT        NOT NULL UNIQUE,
    source_system TEXT        NOT NULL UNIQUE,
    revision      BIGINT      NOT NULL CHECK (revision > 0),
    enabled       BOOLEAN     NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT snaplink_commercial_tenant_valid CHECK (
        octet_length(tenant_id) BETWEEN 1 AND 256
        AND tenant_id = btrim(tenant_id)
        AND tenant_id !~ '[[:cntrl:]]'
    ),
    CONSTRAINT snaplink_commercial_client_valid CHECK (
        octet_length(client_id) BETWEEN 1 AND 512
        AND client_id = btrim(client_id)
        AND client_id !~ '[[:cntrl:]]'
    ),
    CONSTRAINT snaplink_commercial_source_valid CHECK (
        octet_length(source_system) BETWEEN 1 AND 128
        AND source_system = btrim(source_system)
        AND source_system !~ '[[:cntrl:]]'
    ),
    UNIQUE (workspace_id, tenant_id)
);

-- Binding identity is immutable.  An enable/disable desired-state change is a
-- consecutive revision so stale replicas cannot silently re-authorize it.
CREATE OR REPLACE FUNCTION aero_guard_snaplink_binding_update()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
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
        SELECT 1
          FROM snaplink_delivery_outbox outbox
         WHERE outbox.workspace_id = OLD.workspace_id
           AND outbox.client_id = OLD.client_id
           AND outbox.delivered_at IS NULL
    ) THEN
        RAISE EXCEPTION 'Snaplink client cannot rotate with pending deliveries'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'snaplink_binding_client_rotation_pending';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS snaplink_commercial_binding_update_guard
    ON snaplink_commercial_bindings;
CREATE TRIGGER snaplink_commercial_binding_update_guard
    BEFORE UPDATE ON snaplink_commercial_bindings
    FOR EACH ROW
    EXECUTE FUNCTION aero_guard_snaplink_binding_update();

CREATE TABLE IF NOT EXISTS snaplink_entitlement_projections (
    workspace_id            UUID        PRIMARY KEY,
    tenant_id               TEXT        NOT NULL,
    revision                BIGINT      NOT NULL CHECK (revision > 0),
    active                  BOOLEAN     NOT NULL,
    im_enabled              BOOLEAN     NOT NULL,
    notifications_enabled   BOOLEAN     NOT NULL,
    messages_soft           BIGINT      NOT NULL CHECK (messages_soft >= 0),
    messages_hard           BIGINT      NOT NULL CHECK (messages_hard >= 0),
    messages_unlimited      BOOLEAN     NOT NULL,
    notifications_soft      BIGINT      NOT NULL CHECK (notifications_soft >= 0),
    notifications_hard      BIGINT      NOT NULL CHECK (notifications_hard >= 0),
    notifications_unlimited BOOLEAN     NOT NULL,
    effective_at            TIMESTAMPTZ NOT NULL,
    expires_at              TIMESTAMPTZ,
    generated_at            TIMESTAMPTZ NOT NULL,
    projected_at            TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT snaplink_entitlement_binding_fk
        FOREIGN KEY (workspace_id, tenant_id)
        REFERENCES snaplink_commercial_bindings (workspace_id, tenant_id)
        ON DELETE CASCADE,
    CONSTRAINT snaplink_entitlement_messages_limit CHECK (
        (messages_unlimited AND messages_soft = 0 AND messages_hard = 0)
        OR
        (NOT messages_unlimited AND messages_soft <= messages_hard)
    ),
    CONSTRAINT snaplink_entitlement_notifications_limit CHECK (
        (notifications_unlimited AND notifications_soft = 0
         AND notifications_hard = 0)
        OR
        (NOT notifications_unlimited
         AND notifications_soft <= notifications_hard)
    ),
    CONSTRAINT snaplink_entitlement_window CHECK (
        expires_at IS NULL OR expires_at > effective_at
    )
);

CREATE OR REPLACE FUNCTION aero_guard_snaplink_entitlement_revision()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.workspace_id <> OLD.workspace_id
       OR NEW.tenant_id <> OLD.tenant_id
       OR NEW.revision <= OLD.revision THEN
        RAISE EXCEPTION 'Snaplink entitlement projection must advance monotonically'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'snaplink_entitlement_revision_monotonic';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS snaplink_entitlement_revision_guard
    ON snaplink_entitlement_projections;
CREATE TRIGGER snaplink_entitlement_revision_guard
    BEFORE UPDATE ON snaplink_entitlement_projections
    FOR EACH ROW
    EXECUTE FUNCTION aero_guard_snaplink_entitlement_revision();

CREATE TABLE IF NOT EXISTS snaplink_usage_counters (
    workspace_id UUID        NOT NULL,
    dimension    TEXT        NOT NULL CHECK (
        dimension IN ('messages_per_month', 'notifications_per_month')
    ),
    period_start DATE        NOT NULL,
    committed    BIGINT      NOT NULL CHECK (committed >= 0),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (workspace_id, dimension, period_start)
);

-- A single leased outbox carries both destinations.  Identity and source are
-- copied from the trusted server-side binding at enqueue time; request JSON can
-- neither select a tenant nor alter the source.
CREATE TABLE IF NOT EXISTS snaplink_delivery_outbox (
    delivery_id    TEXT        PRIMARY KEY,
    destination    TEXT        NOT NULL CHECK (destination IN ('usage', 'audit')),
    workspace_id   UUID        NOT NULL,
    tenant_id      TEXT        NOT NULL,
    client_id      TEXT        NOT NULL,
    source_system  TEXT        NOT NULL,
    idempotency_key TEXT       NOT NULL,
    payload        JSONB       NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    occurred_at    TIMESTAMPTZ NOT NULL,
    available_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    attempts       BIGINT      NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    claim_token    UUID,
    lease_expires_at TIMESTAMPTZ,
    delivered_at   TIMESTAMPTZ,
    last_error     TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    CONSTRAINT snaplink_delivery_claim_state CHECK (
        (claim_token IS NULL AND lease_expires_at IS NULL)
        OR
        (claim_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    ),
    UNIQUE (destination, idempotency_key)
);

CREATE INDEX IF NOT EXISTS snaplink_delivery_due_idx
    ON snaplink_delivery_outbox (available_at, created_at, delivery_id)
    WHERE delivered_at IS NULL;

CREATE INDEX IF NOT EXISTS snaplink_delivery_workspace_idx
    ON snaplink_delivery_outbox (workspace_id, destination, created_at);

CREATE OR REPLACE FUNCTION aero_snaplink_binding_for_workspace(target_workspace UUID)
RETURNS snaplink_commercial_bindings
LANGUAGE plpgsql
STABLE
AS $$
DECLARE
    binding snaplink_commercial_bindings%ROWTYPE;
BEGIN
    SELECT candidate.* INTO binding
      FROM snaplink_commercial_bindings candidate
     WHERE candidate.workspace_id = target_workspace
       AND candidate.enabled;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'commercial binding is unavailable'
            USING ERRCODE = 'P0001',
                  CONSTRAINT = 'snaplink_binding_required';
    END IF;
    RETURN binding;
END
$$;

CREATE OR REPLACE FUNCTION aero_meter_snaplink_message()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    enforcement_enabled BOOLEAN;
    target_workspace UUID;
    binding snaplink_commercial_bindings%ROWTYPE;
    entitlement snaplink_entitlement_projections%ROWTYPE;
    usage_dimension TEXT;
    usage_feature BOOLEAN;
    usage_hard BIGINT;
    usage_unlimited BOOLEAN;
    period DATE;
    fact_id TEXT;
    committed_after BIGINT;
    trusted_integration BOOLEAN;
BEGIN
    SELECT runtime.enabled INTO enforcement_enabled
      FROM snaplink_commercial_runtime runtime
     WHERE runtime.singleton;
    IF NOT COALESCE(enforcement_enabled, FALSE) THEN
        RETURN NEW;
    END IF;

    SELECT room.workspace_id INTO target_workspace
      FROM rooms room
     WHERE room.id = NEW.room_id;
    IF target_workspace IS NULL THEN
        RAISE EXCEPTION 'message workspace is unavailable'
            USING ERRCODE = 'P0001',
                  CONSTRAINT = 'snaplink_binding_required';
    END IF;
    binding := aero_snaplink_binding_for_workspace(target_workspace);

    SELECT projection.* INTO entitlement
      FROM snaplink_entitlement_projections projection
     WHERE projection.workspace_id = target_workspace
       AND projection.tenant_id = binding.tenant_id
     FOR SHARE;
    IF NOT FOUND
       OR NOT entitlement.active
       OR entitlement.effective_at > clock_timestamp()
       OR (entitlement.expires_at IS NOT NULL
           AND entitlement.expires_at <= clock_timestamp()) THEN
        RAISE EXCEPTION 'commercial entitlement is unavailable'
            USING ERRCODE = 'P0001',
                  CONSTRAINT = 'snaplink_entitlement_required';
    END IF;

    SELECT EXISTS (
        SELECT 1
          FROM integration_installations installation
         WHERE NEW.metadata ->> 'source' = 'integration'
           AND installation.id::text = NEW.metadata ->> 'installation_id'
           AND installation.workspace_id = target_workspace
           AND installation.bot_id = NEW.sender_id
    ) INTO trusted_integration;

    IF trusted_integration THEN
        usage_dimension := 'notifications_per_month';
        usage_feature := entitlement.notifications_enabled;
        usage_hard := entitlement.notifications_hard;
        usage_unlimited := entitlement.notifications_unlimited;
    ELSE
        usage_dimension := 'messages_per_month';
        usage_feature := entitlement.im_enabled;
        usage_hard := entitlement.messages_hard;
        usage_unlimited := entitlement.messages_unlimited;
    END IF;

    IF NOT usage_feature THEN
        RAISE EXCEPTION 'commercial feature is disabled'
            USING ERRCODE = 'P0001',
                  CONSTRAINT = 'snaplink_feature_disabled';
    END IF;
    IF NOT usage_unlimited AND usage_hard = 0 THEN
        RAISE EXCEPTION 'commercial quota is exhausted'
            USING ERRCODE = 'P0001',
                  CONSTRAINT = 'snaplink_quota_exceeded';
    END IF;

    period := date_trunc('month', NEW.created_at AT TIME ZONE 'UTC')::date;
    INSERT INTO snaplink_usage_counters
           (workspace_id, dimension, period_start, committed)
    VALUES (target_workspace, usage_dimension, period, 1)
    ON CONFLICT (workspace_id, dimension, period_start) DO UPDATE
        SET committed = snaplink_usage_counters.committed + 1,
            updated_at = clock_timestamp()
      WHERE usage_unlimited
         OR snaplink_usage_counters.committed < usage_hard
    RETURNING committed INTO committed_after;
    IF committed_after IS NULL THEN
        RAISE EXCEPTION 'commercial quota is exhausted'
            USING ERRCODE = 'P0001',
                  CONSTRAINT = 'snaplink_quota_exceeded';
    END IF;

    fact_id := 'aero-im:' || usage_dimension || ':' || NEW.id::text;
    INSERT INTO snaplink_delivery_outbox
           (delivery_id, destination, workspace_id, tenant_id, client_id,
            source_system, idempotency_key, payload, occurred_at)
    VALUES (
        'usage:' || fact_id,
        'usage',
        target_workspace,
        binding.tenant_id,
        binding.client_id,
        binding.source_system,
        fact_id,
        jsonb_build_object(
            'id', fact_id,
            'dimension', usage_dimension,
            'quantity', 1,
            'occurred_at', NEW.created_at,
            'metadata', jsonb_build_object(
                'workspace_id', target_workspace::text,
                'message_id', NEW.id::text,
                'product', 'aero-im'
            )
        ),
        NEW.created_at
    );
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS messages_snaplink_metering ON messages;
CREATE TRIGGER messages_snaplink_metering
    AFTER INSERT ON messages
    FOR EACH ROW
    EXECUTE FUNCTION aero_meter_snaplink_message();
