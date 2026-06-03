-- 0016_scheduled.sql — Scheduled messages + reminders ("Send later").
--
-- A user composes a message now and it is delivered to a room later; a reminder
-- is just a self-targeted scheduled note (sender == a member of a 1:1 room with
-- themselves, or any room they belong to). The delivery worker polls due rows
-- and replays them through the normal send path.
--
-- Idempotent: safe to re-run.

CREATE TABLE IF NOT EXISTS scheduled_messages (
    id            UUID PRIMARY KEY,
    room_id       UUID NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    sender_id     UUID NOT NULL REFERENCES participants(id) ON DELETE CASCADE,
    blocks        JSONB NOT NULL,
    reply_to      UUID,
    scheduled_at  TIMESTAMPTZ NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at  TIMESTAMPTZ,
    canceled_at   TIMESTAMPTZ
);

-- Due-poll index: the dispatcher claims rows ordered by scheduled_at among those
-- not yet delivered or canceled. Partial so it stays tiny (only pending rows).
CREATE INDEX IF NOT EXISTS scheduled_messages_due_idx
    ON scheduled_messages (scheduled_at)
    WHERE delivered_at IS NULL AND canceled_at IS NULL;

-- A sender's pending list ("my scheduled messages") is filtered by sender and,
-- optionally, room. Partial for the same reason.
CREATE INDEX IF NOT EXISTS scheduled_messages_sender_pending_idx
    ON scheduled_messages (sender_id, room_id)
    WHERE delivered_at IS NULL AND canceled_at IS NULL;
