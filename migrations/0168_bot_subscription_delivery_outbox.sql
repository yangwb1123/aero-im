-- Durable per-subscription webhook delivery for bot event subscriptions.
--
-- Consuming an im.room event only materializes one immutable row per
-- (subscription, producer event). HTTP delivery is performed by an independent
-- leased worker, so completing the NATS consumer receipt cannot lose an event
-- because of a process crash, network/non-2xx response, or attempt-log failure.

CREATE TABLE IF NOT EXISTS bot_subscription_delivery_outbox (
    id                UUID        PRIMARY KEY,
    subscription_id   UUID        NOT NULL
                      REFERENCES bot_event_subscriptions(id) ON DELETE CASCADE,
    bot_id            UUID        NOT NULL,
    event_id          UUID        NOT NULL,
    event_type        TEXT        NOT NULL CHECK (event_type <> ''),
    room_id           UUID        NOT NULL,
    request_body      BYTEA       NOT NULL,
    status            TEXT        NOT NULL DEFAULT 'pending'
                      CHECK (status IN ('pending', 'failed', 'delivered', 'dead')),
    attempts          INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    available_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    claimed_at        TIMESTAMPTZ,
    claim_token       UUID,
    completed_at      TIMESTAMPTZ,
    last_http_status  INTEGER,
    last_error        TEXT CHECK (
        last_error IS NULL OR char_length(last_error) <= 2048
    ),
    created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (subscription_id, event_id),
    CONSTRAINT bot_subscription_delivery_outbox_state_shape CHECK (
        (
            status IN ('pending', 'failed')
            AND completed_at IS NULL
            AND (
                (claimed_at IS NULL AND claim_token IS NULL)
                OR
                (claimed_at IS NOT NULL AND claim_token IS NOT NULL)
            )
        )
        OR
        (
            status IN ('delivered', 'dead')
            AND claimed_at IS NULL
            AND claim_token IS NULL
            AND completed_at IS NOT NULL
        )
    )
);

CREATE INDEX IF NOT EXISTS bot_subscription_delivery_outbox_due_idx
    ON bot_subscription_delivery_outbox (available_at, created_at, id)
    WHERE status IN ('pending', 'failed');

CREATE INDEX IF NOT EXISTS bot_subscription_delivery_outbox_dead_idx
    ON bot_subscription_delivery_outbox (completed_at DESC)
    WHERE status = 'dead';
