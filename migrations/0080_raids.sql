-- 0080_raids.sql — Raids (creator sends their viewers to another stream at end).
--
-- A "raid" is the Twitch/Kick end-of-stream send-off: the source stream's owner
-- raids a target stream, redirecting their current viewers there. We record one
-- row per raid (the source/target streams, who initiated it, and the viewer count
-- carried over) and broadcast a `StreamEvent::Raid` on the SOURCE stream so the
-- viewers' clients can redirect. The actual redirect is client-side; this table
-- is the durable raid log + the per-creator "raids I sent" history.
--
-- Purely additive: a NEW `raid_history` table; no existing table is reshaped, and
-- the whole script is idempotent. `source_stream`/`target_stream` are plain UUID
-- columns (not cascading FKs) so a raid row outlives a pruned stream, mirroring
-- `stream_clips`/`stream_recordings`.
CREATE TABLE IF NOT EXISTS raid_history (
    id            UUID        PRIMARY KEY,
    source_stream UUID        NOT NULL,
    target_stream UUID        NOT NULL,
    raider_id     UUID        NOT NULL,
    viewer_count  INTEGER     NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- "raids sent from this stream", newest first (the GET list path).
CREATE INDEX IF NOT EXISTS raid_history_source_idx
    ON raid_history (source_stream, created_at DESC);

-- "raids this creator initiated", newest first (list_for_creator).
CREATE INDEX IF NOT EXISTS raid_history_raider_idx
    ON raid_history (raider_id, created_at DESC);
