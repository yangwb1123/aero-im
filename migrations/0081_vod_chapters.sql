-- 0081_vod_chapters.sql — VOD chapters / markers (timestamped table-of-contents).
--
-- A VOD (0025, `stream_recordings`) is a finalized live stream. Chapters let the
-- creator annotate timestamps within that VOD ("Intro", "Boss fight", …) so a
-- viewer can jump to a section — playback reuses the VOD's existing HLS playlist
-- with a client-side seek to `start_secs`, no media is processed here. This
-- mirrors how `stream_clips` (0057) annotates ranges over the playlist.
--
-- Purely additive: a NEW `vod_chapters` table; no existing table is reshaped, and
-- the whole script is idempotent. `vod_id` cascades with its recording (a chapter
-- has no meaning without its VOD, unlike a clip which outlives a pruned stream).
-- `start_secs >= 0` and a non-empty `title` are validated at the API layer.
CREATE TABLE IF NOT EXISTS vod_chapters (
    id         UUID        PRIMARY KEY,
    vod_id     UUID        NOT NULL REFERENCES stream_recordings(id) ON DELETE CASCADE,
    start_secs INTEGER     NOT NULL,
    title      TEXT        NOT NULL,
    created_by UUID        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Per-VOD chapter listing, ordered by position in the recording (start_secs).
CREATE INDEX IF NOT EXISTS vod_chapters_vod_idx
    ON vod_chapters (vod_id, start_secs);
