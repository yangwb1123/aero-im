-- 0161_message_send_idempotency.sql — sender-scoped WS message idempotency.
--
-- Client-generated UUIDs map to server-generated, time-sortable MessageIds.
-- The ledger is deliberately separate from `messages`: the latter is being
-- prepared for RANGE partitioning, whose global unique constraints must include
-- the partition key. Keeping the key here also preserves a short-lived
-- deduplication tombstone after an ephemeral message is hard-deleted.

CREATE TABLE IF NOT EXISTS message_send_keys (
    sender_id        UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    client_message_id UUID       NOT NULL,
    message_id       UUID        NOT NULL,
    hash_version     SMALLINT    NOT NULL DEFAULT 1 CHECK (hash_version = 1),
    request_hash     BYTEA       NOT NULL CHECK (octet_length(request_hash) = 32),
    created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (sender_id, client_message_id),
    UNIQUE (message_id)
);

CREATE INDEX IF NOT EXISTS idx_message_send_keys_created
    ON message_send_keys (created_at);
