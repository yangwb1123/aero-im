-- 0025_vod.sql — stream VOD / recording (save a finished live stream for replay).
--
-- A streamer flags a live stream to be recorded; when it ends, the HLS playlist
-- already written by the ingest pipeline (under `server.hls_dir`, pointed at by
-- `streams.hls_path`) is retained as a durable VOD that members can list and play
-- back later. Purely additive: a NEW `stream_recordings` table plus one new
-- nullable/defaulted column on `streams`. No existing table is reshaped, and the
-- whole script is idempotent (safe to re-run).

-- A finalized recording: a snapshot of a stream's playlist taken at finalize time.
-- `stream_id` is intentionally NOT a cascading foreign key — a VOD should outlive
-- its stream row (a stream may be pruned while its recording is kept), so the
-- linkage is a plain column. `owner_id` does reference participants so a VOD is
-- erased when its owner is.
CREATE TABLE IF NOT EXISTS stream_recordings (
    id            UUID        PRIMARY KEY,
    stream_id     UUID        NOT NULL,
    owner_id      UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    room_id       UUID,
    title         TEXT        NOT NULL,
    hls_path      TEXT        NOT NULL,
    duration_secs INTEGER,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Room VOD listing: "this room's recordings, newest first".
CREATE INDEX IF NOT EXISTS stream_recordings_room_idx
    ON stream_recordings (room_id, created_at DESC);

-- Owner VOD listing ("my recordings") + owner-scoped deletes.
CREATE INDEX IF NOT EXISTS stream_recordings_owner_idx
    ON stream_recordings (owner_id);

-- Flag a stream for recording. Additive, nullable-defaulted so existing rows and
-- the existing INSERT path (which does not name this column) keep working.
ALTER TABLE streams
    ADD COLUMN IF NOT EXISTS recording BOOLEAN NOT NULL DEFAULT false;
