-- 0166_consumer_event_receipts.sql
--
-- JetStream's Nats-Msg-Id duplicate window is finite, while a PostgreSQL
-- producer-outbox row can remain pending and retry indefinitely.  Durable
-- side-effect consumers therefore retain their own completion receipt keyed by
-- (consumer, event_id).  A producer retry outside the broker window can then be
-- ACKed without executing the side effect again.

CREATE TABLE IF NOT EXISTS consumer_event_receipts (
    consumer          TEXT        NOT NULL CHECK (
        consumer <> '' AND octet_length(consumer) <= 128
    ),
    event_id          UUID        NOT NULL,
    state             TEXT        NOT NULL CHECK (
        state IN ('processing', 'completed')
    ),
    attempts          INTEGER     NOT NULL DEFAULT 1 CHECK (attempts > 0),
    lease_expires_at  TIMESTAMPTZ,
    completed_at      TIMESTAMPTZ,
    last_error        TEXT        CHECK (
        last_error IS NULL OR char_length(last_error) <= 2048
    ),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (consumer, event_id),
    CONSTRAINT consumer_event_receipts_state_shape CHECK (
        (
            state = 'processing'
            AND lease_expires_at IS NOT NULL
            AND completed_at IS NULL
        )
        OR
        (
            state = 'completed'
            AND lease_expires_at IS NULL
            AND completed_at IS NOT NULL
        )
    )
);

CREATE INDEX IF NOT EXISTS idx_consumer_event_receipts_completed
    ON consumer_event_receipts (completed_at)
    WHERE state = 'completed';

CREATE INDEX IF NOT EXISTS idx_consumer_event_receipts_expired_lease
    ON consumer_event_receipts (lease_expires_at)
    WHERE state = 'processing';
