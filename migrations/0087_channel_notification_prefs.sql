-- 0087_channel_notification_prefs.sql — per-room notification LEVEL (all/mentions/none).
--
-- Slack/Teams-style per-channel notification preference: a participant chooses,
-- for a given room, whether to be notified about EVERY message ('all'), only when
-- @-mentioned ('mentions'), or NEVER ('none'). This is a finer control than the
-- existing binary `channel_mutes` (which 0018 still owns, intact): a 'mentions'
-- row lets through mentions while suppressing the rest, whereas a mute is the
-- equivalent of 'none'. The dispatcher (`ImService::dispatch_notifications`)
-- resolves an effective level per recipient — an explicit row here if present,
-- else 'none' when the room is muted, else 'all' — and suppresses delivery when
-- the level forbids it for that message (mention vs. non-mention).
--
-- The CHECK pins the enumerated levels at the DB so a typo can never persist; the
-- composite PK makes the upsert idempotent and needs no surrogate id.
--
-- Idempotent: safe to re-run.

CREATE TABLE IF NOT EXISTS channel_notification_prefs (
    participant_id uuid NOT NULL,
    room_id        uuid NOT NULL,
    level          text NOT NULL DEFAULT 'all'
        CHECK (level IN ('all', 'mentions', 'none')),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (participant_id, room_id)
);

-- Fan-out read: the dispatcher batches the level for a room's recipients via
-- `room_id = $1 AND participant_id = ANY($2)`; the PK already leads with
-- participant_id, so add a room-leading index for that access pattern.
CREATE INDEX IF NOT EXISTS channel_notification_prefs_room_idx
    ON channel_notification_prefs (room_id, participant_id);
