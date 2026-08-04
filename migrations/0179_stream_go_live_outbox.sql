-- Crash-safe producer outbox for stream.live lifecycle transitions.
--
-- The streams status transition and this immutable snapshot are inserted by one
-- data-modifying statement.  The relay may therefore crash at any point without
-- losing the NATS event or the set of durable webhook deliveries.  There are no
-- foreign keys on the snapshot: deleting a stream, room, or owner must not erase
-- already-committed integration work.
CREATE TABLE IF NOT EXISTS stream_go_live_outbox (
    id                       UUID        PRIMARY KEY,
    event_id                 UUID        NOT NULL UNIQUE,
    stream_id                UUID        NOT NULL,
    room_id                  UUID,
    owner_id                 UUID        NOT NULL,
    title                    TEXT        NOT NULL,
    subject                  TEXT        NOT NULL,
    traceparent              TEXT,
    seq                      BIGINT,
    attempts                 INTEGER     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    claim_token              UUID        NOT NULL DEFAULT gen_random_uuid(),
    available_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at               TIMESTAMPTZ,
    nats_published_at        TIMESTAMPTZ,
    webhooks_materialized_at TIMESTAMPTZ,
    completed_at             TIMESTAMPTZ,
    last_error               TEXT,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (seq IS NULL OR seq >= 0),
    CHECK (
        completed_at IS NULL
        OR (nats_published_at IS NOT NULL AND webhooks_materialized_at IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS stream_go_live_outbox_due_idx
    ON stream_go_live_outbox (available_at, created_at, id)
    WHERE completed_at IS NULL;

CREATE INDEX IF NOT EXISTS stream_go_live_outbox_stream_idx
    ON stream_go_live_outbox (stream_id, created_at DESC);

COMMENT ON TABLE stream_go_live_outbox IS
    'Immutable stream.live snapshots relayed to NATS and durable webhook deliveries';
COMMENT ON COLUMN stream_go_live_outbox.claim_token IS
    'Random generation fence rotated on every lease claim';
