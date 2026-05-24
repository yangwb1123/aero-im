-- P4 互动直播: danmaku (bullet chat) + virtual gifts.
--
-- Both tables cascade-delete with their parent stream. Chat is high-volume and
-- ephemeral-ish (we only ever read a recent window); gifts form a durable ledger
-- that backs the per-stream leaderboard.

CREATE TABLE IF NOT EXISTS stream_chat (
    id          UUID PRIMARY KEY,
    stream_id   UUID NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
    sender_id   UUID NOT NULL REFERENCES participants(id),
    body        TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Recent-window reads: newest-first within a stream.
CREATE INDEX IF NOT EXISTS idx_stream_chat_stream_time
    ON stream_chat (stream_id, created_at DESC);

CREATE TABLE IF NOT EXISTS stream_gifts (
    id          UUID PRIMARY KEY,
    stream_id   UUID NOT NULL REFERENCES streams(id) ON DELETE CASCADE,
    sender_id   UUID NOT NULL REFERENCES participants(id),
    gift_id     TEXT NOT NULL,
    qty         INTEGER NOT NULL CHECK (qty > 0),
    coins       BIGINT  NOT NULL CHECK (coins >= 0),   -- qty * unit price, denormalized
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_stream_gifts_stream_time
    ON stream_gifts (stream_id, created_at DESC);

-- Leaderboard aggregation: GROUP BY (stream_id, sender_id).
CREATE INDEX IF NOT EXISTS idx_stream_gifts_stream_sender
    ON stream_gifts (stream_id, sender_id);
