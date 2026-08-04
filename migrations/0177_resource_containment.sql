-- Bound user-controlled push/bot registry growth and make bot webhook matching
-- tenant-scoped at the SQL candidate-selection edge.

-- Provider tokens are normally much smaller than this. Keeping the byte ceiling
-- below PostgreSQL's btree entry limit also protects the unique token index.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'push_tokens_token_bytes_chk'
           AND conrelid = 'push_tokens'::regclass
    ) THEN
        ALTER TABLE push_tokens
            ADD CONSTRAINT push_tokens_token_bytes_chk
            CHECK (octet_length(token) BETWEEN 1 AND 2048) NOT VALID;
    END IF;
END
$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bots_name_bytes_chk'
           AND conrelid = 'bots'::regclass
    ) THEN
        ALTER TABLE bots
            ADD CONSTRAINT bots_name_bytes_chk
            CHECK (octet_length(name) BETWEEN 1 AND 64) NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bots_icon_url_bytes_chk'
           AND conrelid = 'bots'::regclass
    ) THEN
        ALTER TABLE bots
            ADD CONSTRAINT bots_icon_url_bytes_chk
            CHECK (icon_url IS NULL OR octet_length(icon_url) <= 2048) NOT VALID;
    END IF;
END
$$;

-- IDs use their stable external (ULID) text in filters. Invalid/oversized legacy
-- values project to NULL, so they cannot enter an indexed external-delivery
-- candidate set.
ALTER TABLE bot_event_subscriptions
    ADD COLUMN IF NOT EXISTS scope_room_id TEXT
    GENERATED ALWAYS AS (
        CASE
            WHEN octet_length(NULLIF(filters ->> 'room_id', '')) <= 64
                THEN NULLIF(filters ->> 'room_id', '')
            ELSE NULL
        END
    ) STORED;

ALTER TABLE bot_event_subscriptions
    ADD COLUMN IF NOT EXISTS scope_workspace_id TEXT
    GENERATED ALWAYS AS (
        CASE
            WHEN octet_length(NULLIF(filters ->> 'workspace_id', '')) <= 64
                THEN NULLIF(filters ->> 'workspace_id', '')
            ELSE NULL
        END
    ) STORED;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_sub_event_type_chk'
           AND conrelid = 'bot_event_subscriptions'::regclass
    ) THEN
        ALTER TABLE bot_event_subscriptions
            ADD CONSTRAINT bot_sub_event_type_chk
            CHECK (
                octet_length(event_type) BETWEEN 1 AND 64
                AND event_type ~ '^[a-z0-9_]+$'
            ) NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_sub_filters_size_chk'
           AND conrelid = 'bot_event_subscriptions'::regclass
    ) THEN
        ALTER TABLE bot_event_subscriptions
            ADD CONSTRAINT bot_sub_filters_size_chk
            CHECK (
                jsonb_typeof(COALESCE(filters, '{}'::jsonb)) = 'object'
                AND octet_length(COALESCE(filters, '{}'::jsonb)::text) <= 4096
            ) NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_sub_webhook_url_bytes_chk'
           AND conrelid = 'bot_event_subscriptions'::regclass
    ) THEN
        ALTER TABLE bot_event_subscriptions
            ADD CONSTRAINT bot_sub_webhook_url_bytes_chk
            CHECK (webhook_url IS NULL OR octet_length(webhook_url) <= 2048) NOT VALID;
    END IF;
    IF NOT EXISTS (
        SELECT 1
          FROM pg_constraint
         WHERE conname = 'bot_sub_external_scope_chk'
           AND conrelid = 'bot_event_subscriptions'::regclass
    ) THEN
        ALTER TABLE bot_event_subscriptions
            ADD CONSTRAINT bot_sub_external_scope_chk
            CHECK (
                webhook_url IS NULL
                OR scope_workspace_id IS NOT NULL
            ) NOT VALID;
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS bot_sub_event_room_scope_idx
    ON bot_event_subscriptions (event_type, scope_room_id)
    WHERE webhook_url IS NOT NULL
      AND webhook_secret IS NOT NULL
      AND scope_room_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS bot_sub_event_workspace_scope_idx
    ON bot_event_subscriptions (event_type, scope_workspace_id)
    WHERE webhook_url IS NOT NULL
      AND webhook_secret IS NOT NULL
      AND scope_room_id IS NULL
      AND scope_workspace_id IS NOT NULL;

-- Supports the workspace-wide transactional quota count independently of
-- event type.
CREATE INDEX IF NOT EXISTS bot_sub_external_workspace_quota_idx
    ON bot_event_subscriptions (scope_workspace_id)
    WHERE webhook_url IS NOT NULL
      AND scope_workspace_id IS NOT NULL;
