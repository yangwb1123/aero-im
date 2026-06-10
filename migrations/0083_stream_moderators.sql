-- 0083_stream_moderators.sql — stream moderator role assignment.
--
-- Distinct from `stream_bans` (0026, which records BANNED chatters): this assigns
-- a MOD ROLE to a participant on a stream. A stream owner adds/removes moderators;
-- a moderator gains the same chat-ban/timeout authority as the owner on that
-- stream's danmaku chat (the ban handler now allows owner OR moderator).
--
-- Purely additive: a NEW `stream_moderators` table; no existing table is reshaped,
-- and the whole script is idempotent. One row per (stream, participant) — the
-- UNIQUE constraint makes a re-add a no-op. `stream_id` is a plain column (matching
-- `stream_bans`), so a mod row may outlive a pruned stream; `participant_id` /
-- `created_by` reference participants for referential integrity.
CREATE TABLE IF NOT EXISTS stream_moderators (
    id             UUID        PRIMARY KEY,
    stream_id      UUID        NOT NULL,
    participant_id UUID        NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    created_by     UUID        NOT NULL REFERENCES participants(id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (stream_id, participant_id)
);

-- "this stream's moderators" listing + the per-post is_moderator authority lookup.
CREATE INDEX IF NOT EXISTS stream_moderators_stream_idx
    ON stream_moderators (stream_id);
