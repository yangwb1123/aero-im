-- 0162_event_outbox.sql — transactional producer outbox for room messages.
--
-- A message, its optional sender-scoped idempotency key, and its RoomEvent
-- payload are inserted in one PostgreSQL transaction.  A relay claims the
-- durable row and publishes it to NATS after commit.  `event_id` is stable
-- across retries and is used as the NATS message-id deduplication key.
--
-- `message_id` intentionally has no foreign key: messages are being prepared
-- for RANGE partitioning, and a pending event must survive an independently
-- scheduled message-retention sweep.  The unique constraint still enforces one
-- canonical message-created event per message.

CREATE TABLE IF NOT EXISTS event_outbox (
    id            UUID        PRIMARY KEY,
    event_id      UUID        NOT NULL UNIQUE,
    message_id    UUID        NOT NULL UNIQUE,
    subject       TEXT        NOT NULL CHECK (subject <> ''),
    payload       JSONB       NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
    traceparent   TEXT        CHECK (
        traceparent IS NULL OR octet_length(traceparent) <= 512
    ),
    seq           BIGINT      CHECK (seq IS NULL OR seq >= 0),
    attempts      INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    available_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    claimed_at    TIMESTAMPTZ,
    published_at  TIMESTAMPTZ,
    last_error    TEXT        CHECK (
        last_error IS NULL OR char_length(last_error) <= 2048
    ),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_event_outbox_pending
    ON event_outbox (available_at, created_at, id)
    WHERE published_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_event_outbox_published
    ON event_outbox (published_at)
    WHERE published_at IS NOT NULL;
