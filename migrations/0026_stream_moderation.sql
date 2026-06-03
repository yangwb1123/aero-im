-- 0026_stream_moderation.sql — live-stream chat moderation (bans / timeouts)
--
-- A stream owner can ban (permanent) or time-out (until an expiry) a viewer from
-- posting in that stream's danmaku chat. A banned/timed-out viewer's chat posts
-- are rejected until the ban is lifted (unban) or the timeout expires.
--
-- Purely additive — no existing table is touched. One row per (stream, viewer);
-- re-banning the same viewer upserts. A NULL `until` is a permanent ban; a
-- non-null `until` is a timeout that is only active while `until > now()`.

CREATE TABLE IF NOT EXISTS stream_bans (
    stream_id      UUID        NOT NULL,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    banned_by      UUID        REFERENCES participants(id),
    reason         TEXT,
    -- NULL = permanent ban; non-null = timeout expiry (active while until > now()).
    until          TIMESTAMPTZ,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (stream_id, participant_id)
);

-- Listing a stream's bans + the per-post is_banned lookup both filter by stream.
CREATE INDEX IF NOT EXISTS stream_bans_stream_idx
    ON stream_bans (stream_id);
