-- 0064_call_transcripts.sql — call-transcript persistence + post-call AI recap.
--
-- Teams/Zoom-style "meeting recap": persist a call's final caption lines, then
-- when the call ends produce an AI summary grounded in what was actually said.
-- Reuses the existing P3 caption relay (CallCaption frames) — the WS handler
-- appends each *final* caption line into `call_transcripts`, and the CallEnd
-- handler joins them and stores an AI recap onto the call session.
--
-- `call_id` / `speaker_id` are plain, opaque uuids with NO foreign key — a
-- transcript should outlive the rows it references (a call may be pruned while
-- its transcript is kept), mirroring how `activity_feed` (0062) keeps its
-- subject id a plain column. `recap` hangs off the existing `call_sessions` row
-- (one recap per call). Purely additive: a NEW `call_transcripts` table plus one
-- nullable column; the whole script is idempotent (safe to re-run).
CREATE TABLE IF NOT EXISTS call_transcripts (
    id         UUID        PRIMARY KEY,
    call_id    UUID        NOT NULL,
    speaker_id UUID        NOT NULL,
    text       TEXT        NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Chronological per-call read path: fetch a call's lines in spoken order.
CREATE INDEX IF NOT EXISTS call_transcripts_call_idx
    ON call_transcripts (call_id, created_at);

-- The post-call AI recap (NULL until the call ends and a summary is produced).
ALTER TABLE call_sessions ADD COLUMN IF NOT EXISTS recap text;
