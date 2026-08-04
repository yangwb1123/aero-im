-- Snaplink-authenticated application installations and idempotent IM
-- notification publishing.
--
-- A machine token never becomes an Aero participant.  An administrator binds
-- its exact (issuer, client_id) to one workspace bot and an explicit target
-- policy.  Notification receipts survive client/bot rotation so retries cannot
-- duplicate messages during account or workload migration.

CREATE TABLE IF NOT EXISTS integration_installations (
    id              UUID        PRIMARY KEY,
    workspace_id    UUID        NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    bot_id          UUID        NOT NULL REFERENCES bots(id) ON DELETE RESTRICT,
    issuer          TEXT        NOT NULL,
    client_id       TEXT        NOT NULL,
    name            TEXT        NOT NULL,
    active          BOOLEAN     NOT NULL DEFAULT TRUE,
    -- User-addressed delivery is deliberately opt-in.  A missing JSON field or
    -- a direct SQL insert must never silently widen an installation's target
    -- policy.
    allow_user_dm   BOOLEAN     NOT NULL DEFAULT FALSE,
    created_by      UUID        REFERENCES participants(id) ON DELETE SET NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT integration_installations_issuer_valid CHECK (
        octet_length(issuer) BETWEEN 1 AND 2048
        AND issuer = btrim(issuer)
        AND issuer !~ '[[:cntrl:]]'
    ),
    CONSTRAINT integration_installations_client_valid CHECK (
        octet_length(client_id) BETWEEN 1 AND 512
        AND client_id = btrim(client_id)
        AND client_id !~ '[[:cntrl:]]'
    ),
    CONSTRAINT integration_installations_name_valid CHECK (
        octet_length(name) BETWEEN 1 AND 128
        AND name = btrim(name)
        AND name !~ '[[:cntrl:]]'
    ),
    UNIQUE (workspace_id, issuer, client_id)
);

CREATE INDEX IF NOT EXISTS integration_installations_client_idx
    ON integration_installations (issuer, client_id)
    WHERE active;

CREATE TABLE IF NOT EXISTS integration_installation_rooms (
    installation_id UUID NOT NULL
        REFERENCES integration_installations(id) ON DELETE CASCADE,
    room_id          UUID NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    PRIMARY KEY (installation_id, room_id)
);

CREATE INDEX IF NOT EXISTS integration_installation_rooms_room_idx
    ON integration_installation_rooms (room_id);

CREATE TABLE IF NOT EXISTS integration_notification_receipts (
    installation_id UUID        NOT NULL
        REFERENCES integration_installations(id) ON DELETE CASCADE,
    idempotency_key  UUID        NOT NULL,
    request_hash     BYTEA       NOT NULL CHECK (octet_length(request_hash) = 32),
    target_kind      TEXT        NOT NULL CHECK (target_kind IN ('room', 'snaplink_user')),
    -- Room targets retain the non-personal room UUID.  Snaplink subjects are
    -- never persisted here: the application stores a lowercase SHA-256
    -- fingerprint so durable idempotency cannot become a shadow identity log.
    target_key       TEXT        NOT NULL CHECK (
        (target_kind = 'room'
         AND target_key ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$')
        OR
        (target_kind = 'snaplink_user'
         AND target_key ~ '^sha256:[0-9a-f]{64}$')
    ),
    room_id          UUID        NOT NULL,
    message_id       UUID        NOT NULL,
    outbox_id        UUID        NOT NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at       TIMESTAMPTZ NOT NULL DEFAULT (now() + interval '7 days'),
    PRIMARY KEY (installation_id, idempotency_key),
    UNIQUE (message_id),
    UNIQUE (outbox_id)
);

CREATE INDEX IF NOT EXISTS integration_notification_receipts_expiry_idx
    ON integration_notification_receipts (expires_at, installation_id);

-- Cross-node request coalescing and fencing.  The machine endpoint claims one
-- durable row before it consumes installation/workspace budgets, slow-mode, or
-- object-store capacity.  A crashed owner can be replaced after the lease, but
-- its stale token can no longer commit a message/blob.
CREATE TABLE IF NOT EXISTS integration_machine_requests (
    installation_id UUID        NOT NULL
        REFERENCES integration_installations(id) ON DELETE CASCADE,
    operation        TEXT        NOT NULL CHECK (operation IN ('notification', 'blob')),
    idempotency_key  UUID        NOT NULL,
    request_hash     BYTEA       NOT NULL CHECK (octet_length(request_hash) = 32),
    target_kind      TEXT        NOT NULL CHECK (target_kind IN ('room', 'snaplink_user')),
    target_key       TEXT        NOT NULL CHECK (
        (target_kind = 'room'
         AND target_key ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$')
        OR
        (target_kind = 'snaplink_user'
         AND target_key ~ '^sha256:[0-9a-f]{64}$')
    ),
    status            TEXT        NOT NULL CHECK (status IN ('processing', 'retryable', 'completed')),
    lease_token       UUID,
    lease_expires_at  TIMESTAMPTZ,
    charged_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at        TIMESTAMPTZ NOT NULL DEFAULT (now() + interval '7 days'),
    PRIMARY KEY (installation_id, operation, idempotency_key),
    CONSTRAINT integration_machine_requests_lease_state CHECK (
        (status = 'processing' AND lease_token IS NOT NULL AND lease_expires_at IS NOT NULL)
        OR
        (status <> 'processing' AND lease_token IS NULL AND lease_expires_at IS NULL)
    )
);

CREATE INDEX IF NOT EXISTS integration_machine_requests_rate_idx
    ON integration_machine_requests (installation_id, charged_at DESC);

CREATE INDEX IF NOT EXISTS integration_machine_requests_expiry_idx
    ON integration_machine_requests (expires_at, installation_id)
    WHERE status <> 'processing';

-- Durable storage ownership is intentionally separate from the short
-- idempotency receipt.  It remains chargeable while the blob metadata exists;
-- blob deletion is the only automatic debit (the FK cascade removes the row).
CREATE TABLE IF NOT EXISTS integration_blob_ledger (
    installation_id UUID        NOT NULL
        REFERENCES integration_installations(id) ON DELETE CASCADE,
    blob_id          UUID        NOT NULL REFERENCES blobs(id) ON DELETE CASCADE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (installation_id, blob_id)
);

CREATE INDEX IF NOT EXISTS integration_blob_ledger_blob_idx
    ON integration_blob_ledger (blob_id);

-- Blob receipts make upload retries canonical just like notification receipts.
-- The subject fingerprint follows the same privacy rule as notifications.  A
-- receipt intentionally retains its blob: deleting the installation first
-- cascades the receipt and releases the normal blob lifecycle again.
CREATE TABLE IF NOT EXISTS integration_blob_receipts (
    installation_id UUID        NOT NULL
        REFERENCES integration_installations(id) ON DELETE CASCADE,
    idempotency_key  UUID        NOT NULL,
    request_hash     BYTEA       NOT NULL CHECK (octet_length(request_hash) = 32),
    target_kind      TEXT        NOT NULL CHECK (target_kind IN ('room', 'snaplink_user')),
    target_key       TEXT        NOT NULL CHECK (
        (target_kind = 'room'
         AND target_key ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$')
        OR
        (target_kind = 'snaplink_user'
         AND target_key ~ '^sha256:[0-9a-f]{64}$')
    ),
    room_id          UUID        NOT NULL,
    blob_id          UUID        NOT NULL REFERENCES blobs(id) ON DELETE RESTRICT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at       TIMESTAMPTZ NOT NULL DEFAULT (now() + interval '7 days'),
    PRIMARY KEY (installation_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS integration_blob_receipts_blob_idx
    ON integration_blob_receipts (blob_id);

CREATE INDEX IF NOT EXISTS integration_blob_receipts_expiry_idx
    ON integration_blob_receipts (expires_at, installation_id);

-- Installation/workspace erasure must not strand unreferenced Vault objects.
-- Preserve a stronger pre-existing GDPR queue policy when present.
CREATE OR REPLACE FUNCTION aero_enqueue_integration_blobs_on_delete()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO blob_gc_queue (blob_id, force_delete)
    SELECT ledger.blob_id, FALSE
      FROM integration_blob_ledger ledger
     WHERE ledger.installation_id = OLD.id
       AND NOT EXISTS (
             SELECT 1 FROM integration_blob_ledger other
              WHERE other.blob_id = ledger.blob_id
                AND other.installation_id <> OLD.id
       )
       AND NOT EXISTS (
             SELECT 1 FROM integration_blob_receipts receipt
              WHERE receipt.blob_id = ledger.blob_id
                AND receipt.installation_id <> OLD.id
       )
    ON CONFLICT (blob_id) DO UPDATE
        SET force_delete = blob_gc_queue.force_delete OR EXCLUDED.force_delete;
    RETURN OLD;
END
$$;

DROP TRIGGER IF EXISTS integration_installation_blob_gc
    ON integration_installations;
CREATE TRIGGER integration_installation_blob_gc
    BEFORE DELETE ON integration_installations
    FOR EACH ROW
    EXECUTE FUNCTION aero_enqueue_integration_blobs_on_delete();

-- Database-boundary tenant guards. Runtime publish checks repeat effective
-- account/membership policy because those properties are intentionally mutable.
CREATE OR REPLACE FUNCTION aero_guard_integration_installation_bot()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM bots bot
          JOIN participants participant
            ON participant.id = bot.id
           AND participant.kind = 'bot'
           AND participant.deleted_at IS NULL
          JOIN workspace_members membership
            ON membership.workspace_id = NEW.workspace_id
           AND membership.participant_id = bot.id
          LEFT JOIN workspace_deactivations deactivated
            ON deactivated.workspace_id = NEW.workspace_id
           AND deactivated.participant_id = bot.id
         WHERE bot.id = NEW.bot_id
           AND bot.workspace_id = NEW.workspace_id
           AND deactivated.participant_id IS NULL
    ) THEN
        RAISE EXCEPTION 'integration bot must be an active member of its workspace'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'integration_installation_bot_scope';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS integration_installation_bot_guard
    ON integration_installations;
CREATE TRIGGER integration_installation_bot_guard
    BEFORE INSERT OR UPDATE OF workspace_id, bot_id
    ON integration_installations
    FOR EACH ROW
    EXECUTE FUNCTION aero_guard_integration_installation_bot();

CREATE OR REPLACE FUNCTION aero_guard_integration_room_scope()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM integration_installations installation
          JOIN rooms room
            ON room.id = NEW.room_id
           AND room.workspace_id = installation.workspace_id
          JOIN room_members member
            ON member.room_id = room.id
           AND member.participant_id = installation.bot_id
         WHERE installation.id = NEW.installation_id
    ) THEN
        RAISE EXCEPTION 'integration room must belong to the workspace and contain its bot'
            USING ERRCODE = '23514',
                  CONSTRAINT = 'integration_installation_room_scope';
    END IF;
    RETURN NEW;
END
$$;

DROP TRIGGER IF EXISTS integration_installation_room_guard
    ON integration_installation_rooms;
CREATE TRIGGER integration_installation_room_guard
    BEFORE INSERT OR UPDATE OF installation_id, room_id
    ON integration_installation_rooms
    FOR EACH ROW
    EXECUTE FUNCTION aero_guard_integration_room_scope();
