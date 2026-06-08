-- 0057_clips.sql — live-stream clips (Twitch/YouTube-style shareable highlights).
--
-- A viewer marks a timestamped [start_secs, end_secs] range of a live stream / VOD
-- to share. Playback reuses the existing HLS playlist with client-side seek to the
-- range — no media processing happens here, so a clip is just metadata pointing at
-- a stream plus an in/out point. Purely additive: a NEW `stream_clips` table; no
-- existing table is reshaped, and the whole script is idempotent (safe to re-run).
--
-- `stream_id` is intentionally NOT a cascading foreign key — a clip should outlive
-- its stream row (a stream may be pruned while its clip is kept), mirroring how
-- `stream_recordings` (0025) keeps `stream_id` a plain column. The range bounds
-- (0 <= start < end and a max duration) are validated at the API layer.
CREATE TABLE IF NOT EXISTS stream_clips (
    id         UUID        PRIMARY KEY,
    stream_id  UUID        NOT NULL,
    creator_id UUID        NOT NULL,
    title      TEXT        NOT NULL,
    start_secs INTEGER     NOT NULL,
    end_secs   INTEGER     NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Per-stream clip listing: "this stream's clips" (the GET list path).
CREATE INDEX IF NOT EXISTS stream_clips_stream_idx ON stream_clips (stream_id);
